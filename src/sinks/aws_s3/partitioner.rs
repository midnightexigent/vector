use crate::{internal_events::TemplateRenderingFailed, template::Template};
use vector_core::{event::Event, partition::Partitioner};

/// Partitions items based on the generated S3 object key for the given event.
///
/// TODO: Realistically, this could be a generic "template partitioner", since
/// I'm guessing other sinks might want to partition based on a key generated
/// from event data.
pub struct KeyPartitioner(Template);

impl KeyPartitioner {
    pub const fn new(template: Template) -> Self {
        Self(template)
    }
}

impl Partitioner for KeyPartitioner {
    type Item = Event;
    type Key = Option<String>;

    fn partition(&self, item: &Self::Item) -> Self::Key {
        self.0
            .render_string(item)
            .map_err(|error| {
                emit!(TemplateRenderingFailed {
                    error,
                    field: Some("key_prefix"),
                    drop_event: true,
                });
            })
            .ok()
    }
}