use bytes::Bytes;
use nexkvm_input::{InputCapture, InputError, InputEvent, InputInjector};
use nexkvm_network::{Connection, NetworkError};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum InputSessionError {
    #[error("input payload codec error: {0}")]
    Codec(String),
    #[error("unexpected message kind: {0:?}")]
    UnexpectedKind(MessageKind),
}

#[allow(dead_code)]
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

impl From<NetworkError> for InputSessionError {
    fn from(error: NetworkError) -> Self {
        Self::Codec(error.to_string())
    }
}

impl From<InputError> for InputSessionError {
    fn from(error: InputError) -> Self {
        Self::Codec(error.to_string())
    }
}

#[allow(dead_code)]
pub async fn forward_n_events<C, K>(
    capture: &C,
    connection: &K,
    first_id: MessageId,
    count: usize,
) -> Result<MessageId, InputSessionError>
where
    C: InputCapture + ?Sized,
    K: Connection + ?Sized,
{
    let mut next_id = first_id;
    for _ in 0..count {
        let event = capture.next_event().await?;
        connection.send(encode_input_event(next_id, event)).await?;
        next_id = next_id.next();
    }
    Ok(next_id)
}

pub async fn forward_until_error<C, K>(
    capture: &C,
    connection: &K,
    first_id: MessageId,
) -> Result<(), InputSessionError>
where
    C: InputCapture + ?Sized,
    K: Connection + ?Sized,
{
    let mut next_id = first_id;
    loop {
        let event = capture.next_event().await?;
        connection.send(encode_input_event(next_id, event)).await?;
        next_id = next_id.next();
    }
}

pub async fn inject_until_closed<K, I>(
    connection: &K,
    injector: &I,
) -> Result<(), InputSessionError>
where
    K: Connection + ?Sized,
    I: InputInjector + ?Sized,
{
    loop {
        match connection.recv().await {
            Ok(envelope) => {
                if envelope.kind != MessageKind::Input {
                    continue;
                }
                injector.inject(decode_input_event(envelope)?).await?;
            }
            Err(NetworkError::Closed) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRuntimeRole {
    Disabled,
    Source,
    Target,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRuntimePlan {
    pub start_capture_forwarder: bool,
    pub start_inject_receiver: bool,
}

pub fn plan_runtime(role: InputRuntimeRole, permissions_ready: bool) -> InputRuntimePlan {
    if !permissions_ready {
        return InputRuntimePlan {
            start_capture_forwarder: false,
            start_inject_receiver: false,
        };
    }
    match role {
        InputRuntimeRole::Disabled => InputRuntimePlan {
            start_capture_forwarder: false,
            start_inject_receiver: false,
        },
        InputRuntimeRole::Source => InputRuntimePlan {
            start_capture_forwarder: true,
            start_inject_receiver: false,
        },
        InputRuntimeRole::Target => InputRuntimePlan {
            start_capture_forwarder: false,
            start_inject_receiver: true,
        },
        InputRuntimeRole::Both => InputRuntimePlan {
            start_capture_forwarder: true,
            start_inject_receiver: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use nexkvm_network::{Connection, NetworkError, TransportKind};
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

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

    #[derive(Debug)]
    struct QueueCapture {
        events: Mutex<VecDeque<Result<InputEvent, InputError>>>,
    }

    impl QueueCapture {
        fn new(events: Vec<InputEvent>) -> Self {
            Self {
                events: Mutex::new(events.into_iter().map(Ok).collect()),
            }
        }
    }

    #[async_trait]
    impl InputCapture for QueueCapture {
        async fn next_event(&self) -> Result<InputEvent, InputError> {
            self.events
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(InputError::Backend("empty capture queue".into())))
        }
    }

    #[derive(Debug, Default)]
    struct RecordingInjector {
        events: Mutex<Vec<InputEvent>>,
    }

    #[async_trait]
    impl InputInjector for RecordingInjector {
        async fn inject(&self, event: InputEvent) -> Result<(), InputError> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct MemoryConnection {
        sent: Mutex<Vec<Envelope>>,
        recv: Mutex<VecDeque<Envelope>>,
    }

    impl MemoryConnection {
        fn with_recv(envelopes: Vec<Envelope>) -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                recv: Mutex::new(envelopes.into()),
            }
        }
    }

    #[async_trait]
    impl Connection for MemoryConnection {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        fn peer_addr(&self) -> SocketAddr {
            "127.0.0.1:47654".parse().unwrap()
        }

        async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
            self.sent.lock().unwrap().push(envelope);
            Ok(())
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            self.recv
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(NetworkError::Closed)
        }

        async fn close(&self) -> Result<(), NetworkError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn forwards_captured_events_to_connection() {
        let capture = QueueCapture::new(vec![
            InputEvent::KeyPress(0x04),
            InputEvent::KeyRelease(0x04),
        ]);
        let connection = Arc::new(MemoryConnection::default());

        forward_n_events(&capture, &*connection, MessageId(10), 2)
            .await
            .unwrap();

        let sent = connection.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].id, MessageId(10));
        assert_eq!(
            decode_input_event(sent[0].clone()).unwrap(),
            InputEvent::KeyPress(0x04)
        );
        assert_eq!(sent[1].id, MessageId(11));
        assert_eq!(
            decode_input_event(sent[1].clone()).unwrap(),
            InputEvent::KeyRelease(0x04)
        );
    }

    #[tokio::test]
    async fn forward_until_error_sends_until_capture_stops() {
        let capture = QueueCapture::new(vec![
            InputEvent::KeyPress(0x04),
            InputEvent::KeyRelease(0x04),
        ]);
        let connection = Arc::new(MemoryConnection::default());

        let error = forward_until_error(&capture, &*connection, MessageId(20))
            .await
            .unwrap_err();

        assert!(matches!(error, InputSessionError::Codec(_)));
        let sent = connection.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].id, MessageId(20));
        assert_eq!(sent[1].id, MessageId(21));
    }

    #[tokio::test]
    async fn injects_received_input_envelopes() {
        let injector = RecordingInjector::default();
        let connection = MemoryConnection::with_recv(vec![
            encode_input_event(
                MessageId(1),
                InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
            ),
            encode_input_event(
                MessageId(2),
                InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
            ),
        ]);

        inject_until_closed(&connection, &injector).await.unwrap();

        assert_eq!(
            injector.events.lock().unwrap().as_slice(),
            &[
                InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
                InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
            ]
        );
    }

    #[test]
    fn target_role_starts_receiver_only_when_permissions_are_ready() {
        assert_eq!(
            plan_runtime(InputRuntimeRole::Target, true),
            InputRuntimePlan {
                start_capture_forwarder: false,
                start_inject_receiver: true,
            }
        );
        assert_eq!(
            plan_runtime(InputRuntimeRole::Target, false),
            InputRuntimePlan {
                start_capture_forwarder: false,
                start_inject_receiver: false,
            }
        );
    }

    #[test]
    fn source_role_starts_capture_only_when_permissions_are_ready() {
        assert_eq!(
            plan_runtime(InputRuntimeRole::Source, true),
            InputRuntimePlan {
                start_capture_forwarder: true,
                start_inject_receiver: false,
            }
        );
    }
}
