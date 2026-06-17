use bytes::Bytes;
use nexkvm_input::InputEvent;
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum InputSessionError {
    #[error("input payload codec error: {0}")]
    Codec(String),
    #[error("unexpected message kind: {0:?}")]
    UnexpectedKind(MessageKind),
}

pub fn encode_input_event(id: MessageId, event: InputEvent) -> Envelope {
    let body = serde_json::to_vec(&event).expect("InputEvent serialization is infallible");
    Envelope::new(PROTOCOL_VERSION, id, MessageKind::Input, Bytes::from(body))
}

pub fn decode_input_event(envelope: Envelope) -> Result<InputEvent, InputSessionError> {
    if envelope.kind != MessageKind::Input {
        return Err(InputSessionError::UnexpectedKind(envelope.kind));
    }
    serde_json::from_slice(&envelope.body)
        .map_err(|error| InputSessionError::Codec(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_event_round_trips_through_envelope_body() {
        let event = InputEvent::KeyPress(0x04);
        let envelope = encode_input_event(MessageId(7), event);

        assert_eq!(envelope.version, PROTOCOL_VERSION);
        assert_eq!(envelope.id, MessageId(7));
        assert_eq!(envelope.kind, MessageKind::Input);
        assert_eq!(decode_input_event(envelope).unwrap(), event);
    }

    #[test]
    fn rejects_non_input_envelopes() {
        let envelope = Envelope::new(
            PROTOCOL_VERSION,
            MessageId(1),
            MessageKind::Clipboard,
            Bytes::from_static(b"not input"),
        );

        assert!(matches!(
            decode_input_event(envelope),
            Err(InputSessionError::UnexpectedKind(MessageKind::Clipboard))
        ));
    }
}
