//! Which worker, generation and run an event belongs to, and its sanitiser key.

use super::ids::{ProcessGeneration, RunId, WorkerId};
use crate::event::{Envelope, StreamKind};
use crate::redact::stream::StreamKey;

/// Where an event came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Scope {
    pub root: WorkerId,
    pub worker: WorkerId,
    pub generation: ProcessGeneration,
    pub run: Option<RunId>,
}

/// An event with its scope.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedEnvelope {
    pub scope: Scope,
    pub envelope: Envelope,
}

fn stream_name(stream: StreamKind) -> &'static str {
    match stream {
        StreamKind::Assistant => "assistant",
        StreamKind::Reasoning => "reasoning",
        StreamKind::ToolInput => "tool_input",
        StreamKind::ToolOutput => "tool_output",
    }
}

impl Scope {
    /// The concrete key: worker handle, generation, `Envelope.item` and the stream kind in
    /// snake_case.
    pub fn stream_key(&self, item: &str, stream: StreamKind) -> StreamKey {
        StreamKey {
            worker: self.worker.as_str().to_owned(),
            generation: self.generation.0,
            item: item.to_owned(),
            stream: stream_name(stream).to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(generation: u64) -> Scope {
        Scope {
            root: WorkerId::parse("w_rootroot").expect("valid"),
            worker: WorkerId::parse("w_abcdefgh").expect("valid"),
            generation: ProcessGeneration(generation),
            run: None,
        }
    }

    #[test]
    fn stream_key_has_the_four_documented_fields() {
        let key = scope(3).stream_key("item-1", StreamKind::ToolOutput);
        assert_eq!(
            key,
            StreamKey {
                worker: "w_abcdefgh".into(),
                generation: 3,
                item: "item-1".into(),
                stream: "tool_output".into(),
            }
        );
        assert_ne!(key, scope(4).stream_key("item-1", StreamKind::ToolOutput));
        // The stream name is StreamKind's own serde name.
        for kind in [
            StreamKind::Assistant,
            StreamKind::Reasoning,
            StreamKind::ToolInput,
            StreamKind::ToolOutput,
        ] {
            assert_eq!(
                serde_json::to_value(kind).expect("ser"),
                serde_json::json!(stream_name(kind))
            );
        }
    }
}
