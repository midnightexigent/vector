use crate::{
    config::{DataType, GenerateConfig, SinkConfig, SinkContext},
    sinks::{
        util::{
            encoding::{EncodingConfig, EncodingConfiguration},
            sink::Response,
            BatchConfig, BatchSettings, Buffer, Compression, PartitionBatchSink, PartitionBuffer,
            PartitionInnerBuffer, ServiceBuilderExt, TowerRequestConfig,
        },
        Healthcheck, VectorSink,
    },
    template::Template,
    Event, Result,
};
use azure_sdk_core::{
    errors::AzureError, BlobNameSupport, BodySupport, ContainerNameSupport, ContentEncodingSupport,
    ContentTypeSupport,
};
use azure_sdk_storage_blob::{blob::responses::PutBlockBlobResponse, Blob, Container};
use azure_sdk_storage_core::{key_client::KeyClient, prelude::client::from_connection_string};
use bytes::Bytes;
use chrono::Utc;
use futures::{future::BoxFuture, stream, FutureExt, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    convert::TryFrom,
    result::Result as StdResult,
    task::{Context, Poll},
};
use tower::{Service, ServiceBuilder};

#[derive(Clone)]
pub struct AzureBlobSink {
    client: KeyClient,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct AzureBlobSinkConfig {
    pub connection_string: Option<String>,
    pub container_name: Option<String>,
    pub blob_prefix: Option<String>,
    pub blob_time_format: Option<String>,
    pub encoding: EncodingConfig<Encoding>,
    #[serde(default = "Compression::gzip_default")]
    pub compression: Compression,
    #[serde(default)]
    pub batch: BatchConfig,
    #[serde(default)]
    pub request: TowerRequestConfig,
}

#[derive(Debug, Clone)]
struct AzureBlobSinkRequest {
    container_name: String,
    blob_name: String,
    blob_data: Vec<u8>,
    content_encoding: Option<&'static str>,
}

#[derive(Deserialize, Serialize, Debug, Eq, PartialEq, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Json,
}

impl GenerateConfig for AzureBlobSinkConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(Self {
            connection_string: None,
            container_name: Option::Some(String::from("logs")),
            blob_prefix: None,
            blob_time_format: None,
            encoding: Encoding::Json.into(),
            compression: Compression::gzip_default(),
            batch: BatchConfig::default(),
            request: TowerRequestConfig::default(),
        })
        .unwrap()
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "azure_blob")]
impl SinkConfig for AzureBlobSinkConfig {
    async fn build(&self, cx: SinkContext) -> Result<(VectorSink, Healthcheck)> {
        let client = self.create_client()?;
        let healthcheck = self.clone().healthcheck(client.clone()).boxed();
        let sink = self.new(client, cx)?;
        Ok((sink, healthcheck))
    }

    fn input_type(&self) -> DataType {
        DataType::Log
    }

    fn sink_type(&self) -> &'static str {
        "azure_blob"
    }
}

impl AzureBlobSinkConfig {
    pub fn new(&self, client: KeyClient, cx: SinkContext) -> Result<VectorSink> {
        let batch = BatchSettings::default()
            .bytes(10 * 1024 * 1024)
            .timeout(300)
            .parse_config(self.batch)?;
        let compression = self.compression.clone();
        let container_name = self.container_name.clone().unwrap();
        let blob_time_format = self.blob_time_format.clone().unwrap();
        let blob = AzureBlobSink { client };
        let svc = ServiceBuilder::new()
            .map(move |request| {
                build_request(
                    request,
                    compression,
                    container_name.clone(),
                    blob_time_format.clone(),
                )
            })
            .service(blob);

        let encoding = self.encoding.clone();
        let blob_prefix = self.blob_prefix.as_deref().unwrap();
        let blob_prefix_template = Template::try_from(blob_prefix)?;
        let buffer = PartitionBuffer::new(Buffer::new(batch.size, compression));
        let sink = PartitionBatchSink::new(svc, buffer, batch.timeout, cx.acker())
            .with_flat_map(move |event| {
                stream::iter(encode_event(event, &blob_prefix_template, &encoding)).map(Ok)
            })
            .sink_map_err(|error| error!(message = "Sink failed to flush.", %error));

        Ok(super::VectorSink::Sink(Box::new(sink)))
    }

    pub async fn healthcheck(self, client: KeyClient) -> Result<()> {
        let container_name = self.container_name.clone().unwrap();
        let request = client
            .get_container_properties()
            .with_container_name(container_name.as_str())
            .finalize();

        // todo: map error to health check error
        match request.await {
            Ok(_) => Ok(()),
            Err(error) => Err(Box::new(error)),
        }
    }

    pub fn create_client(&self) -> Result<KeyClient> {
        let connection_string = self.connection_string.clone().unwrap();
        let client = from_connection_string(connection_string.as_str())?;

        Ok(client)
    }
}

impl Service<AzureBlobSinkRequest> for AzureBlobSink {
    type Response = PutBlockBlobResponse;
    type Error = AzureError;
    type Future = BoxFuture<'static, StdResult<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<StdResult<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: AzureBlobSinkRequest) -> Self::Future {
        let client = self.client.clone();
        let container_name = request.container_name.clone();
        let blob_name = request.blob_name.clone();
        let blob_data = request.blob_data.clone();

        Box::pin(async move {
            client
                .put_block_blob()
                .with_container_name(container_name.as_str())
                .with_blob_name(blob_name.as_str())
                .with_body(blob_data.as_slice())
                .with_content_encoding(request.content_encoding.unwrap())
                // todo: remove hardcoded value
                .with_content_type("text/x-log")
                .finalize()
                .await
        })
    }
}

impl Response for PutBlockBlobResponse {}

fn encode_event(
    mut event: Event,
    blob_prefix: &Template,
    encoding: &EncodingConfig<Encoding>,
) -> Option<PartitionInnerBuffer<Vec<u8>, Bytes>> {
    let key = blob_prefix
        .render_string(&event)
        .map_err(|missing_keys| {
            warn!(
                message = "Keys do not exist on the event; dropping event.",
                ?missing_keys,
                internal_log_rate_secs = 30,
            );
        })
        .ok()?;

    encoding.apply_rules(&mut event);

    let log = event.into_log();
    let bytes = match encoding.codec() {
        Encoding::Json => {
            serde_json::to_vec(&log).expect("Failed to encode event as json, this is a bug!")
        }
    };

    Some(PartitionInnerBuffer::new(bytes, key.into()))
}

fn build_request(
    request: PartitionInnerBuffer<Vec<u8>, Bytes>,
    compression: Compression,
    container_name: String,
    blob_time_format: String,
) -> AzureBlobSinkRequest {
    let (inner, key) = request.into_parts();
    let filename = Utc::now().format(&blob_time_format).to_string();
    let blob = String::from_utf8_lossy(&key[..]).into_owned();
    let blob = format!("{}{}.{}", blob, filename, compression.extension());

    debug!(
        message = "Sending events.",
        bytes = ?inner.len(),
        container = ?container_name,
        blob = ?blob
    );

    AzureBlobSinkRequest {
        container_name,
        blob_data: inner,
        blob_name: blob,
        content_encoding: compression.content_encoding(),
    }
}