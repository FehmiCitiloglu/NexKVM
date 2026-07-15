use bytes::Bytes;
use nexkvm_core::identity::DeviceId;
use nexkvm_input::{
    BoundaryDetector, DisplayRect, Edge, EdgeLink, InputCapture, InputError, InputEvent,
    InputInjector, MonitorId, MonitorLayout, MouseShareController, ShareOutput,
};
use nexkvm_network::{Connection, NetworkError};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum InputSessionError {
    #[error("emergency stop requested")]
    EmergencyStop,
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

#[cfg(test)]
async fn forward_until_error<C, K>(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffEdge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg(test)]
pub struct ExtendedInputShare {
    edge: HandoffEdge,
    emergency_stop_keycode: u32,
    focus: ShareFocus,
    last_local_pos: Option<(f64, f64)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg(test)]
enum ShareFocus {
    Local,
    Remote { pos: (f64, f64) },
}

#[cfg(test)]
impl ExtendedInputShare {
    pub fn new(edge: HandoffEdge, emergency_stop_keycode: u32) -> Self {
        Self {
            edge,
            emergency_stop_keycode,
            focus: ShareFocus::Local,
            last_local_pos: None,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.focus, ShareFocus::Remote { .. })
    }

    pub fn release_remote(&mut self) -> bool {
        let was_remote = self.is_remote();
        self.focus = ShareFocus::Local;
        self.last_local_pos = None;
        was_remote
    }

    pub fn route(&mut self, event: InputEvent) -> Option<InputEvent> {
        match self.focus {
            ShareFocus::Local => self.route_local(event),
            ShareFocus::Remote { pos } => self.route_remote(event, pos),
        }
    }

    fn route_local(&mut self, event: InputEvent) -> Option<InputEvent> {
        let InputEvent::PointerMove { x, y } = event else {
            return None;
        };
        self.last_local_pos = Some((x, y));
        if !at_handoff_edge(self.edge, x, y) {
            return None;
        }
        let entry = entry_for_edge(self.edge, x, y);
        self.focus = ShareFocus::Remote { pos: entry };
        Some(InputEvent::PointerMove {
            x: entry.0,
            y: entry.1,
        })
    }

    fn route_remote(&mut self, event: InputEvent, pos: (f64, f64)) -> Option<InputEvent> {
        match event {
            InputEvent::KeyPress(keycode) if keycode == self.emergency_stop_keycode => {
                self.release_remote();
                None
            }
            InputEvent::RelativeMove { dx, dy } => self.advance_remote_pointer(pos, dx, dy),
            InputEvent::PointerMove { x, y } => {
                let (last_x, last_y) = self.last_local_pos.unwrap_or((x, y));
                self.last_local_pos = Some((x, y));
                self.advance_remote_pointer(pos, x - last_x, y - last_y)
            }
            other => Some(other),
        }
    }

    fn advance_remote_pointer(&mut self, pos: (f64, f64), dx: f64, dy: f64) -> Option<InputEvent> {
        let next = (pos.0 + dx, pos.1 + dy);
        if returned_to_local(self.edge, next.0, next.1) {
            self.focus = ShareFocus::Local;
            return None;
        }
        let clamped = (next.0.clamp(0.0, 1.0), next.1.clamp(0.0, 1.0));
        self.focus = ShareFocus::Remote { pos: clamped };
        Some(InputEvent::PointerMove {
            x: clamped.0,
            y: clamped.1,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LinkedScreenInputShare {
    controller: MouseShareController,
    edge: HandoffEdge,
    emergency_stop_keycode: u32,
    last_local_pos: Option<(f64, f64)>,
}

impl LinkedScreenInputShare {
    pub fn single_peer(edge: HandoffEdge, emergency_stop_keycode: u32) -> Self {
        let peer = DeviceId::generate();
        let local_layout =
            MonitorLayout::new(vec![(MonitorId(0), DisplayRect::new(0, 0, 1000, 1000))]);
        let boundary = BoundaryDetector::new(
            DisplayRect::new(0, 0, 1000, 1000),
            vec![EdgeLink {
                edge: linked_edge(edge),
                peer,
            }],
        );
        Self {
            controller: MouseShareController::new(boundary, local_layout),
            edge,
            emergency_stop_keycode,
            last_local_pos: None,
        }
    }

    pub fn is_remote(&self) -> bool {
        self.controller.active_peer().is_some()
    }

    pub fn release_remote(&mut self) -> bool {
        self.last_local_pos = None;
        self.controller.release_remote()
    }

    pub fn route(&mut self, event: InputEvent) -> Option<InputEvent> {
        if matches!(event, InputEvent::KeyPress(keycode) if keycode == self.emergency_stop_keycode)
        {
            self.release_remote();
            return None;
        }

        if !self.is_remote() {
            return self.route_local(event);
        }

        self.route_remote(event)
    }

    fn route_local(&mut self, event: InputEvent) -> Option<InputEvent> {
        let InputEvent::PointerMove { x, y } = event else {
            return None;
        };
        self.last_local_pos = Some((x, y));
        let (px, py) = boundary_sample_for_edge(self.edge, x, y);
        match self.controller.on_local_cursor(px, py) {
            ShareOutput::EnterRemote(entry) => Some(entry.entry_event()),
            _ => None,
        }
    }

    fn route_remote(&mut self, event: InputEvent) -> Option<InputEvent> {
        match event {
            InputEvent::RelativeMove { dx, dy } => self.route_remote_motion(dx, dy),
            InputEvent::PointerMove { x, y } => {
                let (last_x, last_y) = self.last_local_pos.unwrap_or((x, y));
                self.last_local_pos = Some((x, y));
                self.route_remote_motion(x - last_x, y - last_y)
            }
            other => Some(other),
        }
    }

    fn route_remote_motion(&mut self, dx: f64, dy: f64) -> Option<InputEvent> {
        match self.controller.on_remote_motion(dx, dy) {
            ShareOutput::Forward { event, .. } => Some(event),
            ShareOutput::ReturnLocal { .. } => {
                self.last_local_pos = None;
                None
            }
            ShareOutput::Idle | ShareOutput::EnterRemote(_) => None,
        }
    }
}

fn linked_edge(edge: HandoffEdge) -> Edge {
    match edge {
        HandoffEdge::Left => Edge::Left,
        HandoffEdge::Right => Edge::Right,
        HandoffEdge::Top => Edge::Top,
        HandoffEdge::Bottom => Edge::Bottom,
    }
}

fn normalized_to_default_pixels(x: f64, y: f64) -> (i32, i32) {
    (
        (x.clamp(0.0, 1.0) * 1000.0).round() as i32,
        (y.clamp(0.0, 1.0) * 1000.0).round() as i32,
    )
}

fn boundary_sample_for_edge(edge: HandoffEdge, x: f64, y: f64) -> (i32, i32) {
    let (mut px, mut py) = normalized_to_default_pixels(x, y);
    if at_handoff_edge(edge, x, y) {
        match edge {
            HandoffEdge::Left => px = -1,
            HandoffEdge::Right => px = 1000,
            HandoffEdge::Top => py = -1,
            HandoffEdge::Bottom => py = 1000,
        }
    }
    (px, py)
}

pub async fn forward_extended_until_error<C, K, S>(
    capture: &C,
    connection: &K,
    first_id: MessageId,
    edge: HandoffEdge,
    emergency_stop_keycode: u32,
    remote_focus_timeout_millis: u64,
    mut set_suppressed: S,
) -> Result<(), InputSessionError>
where
    C: InputCapture + ?Sized,
    K: Connection + ?Sized,
    S: FnMut(bool),
{
    let mut next_id = first_id;
    let mut share = LinkedScreenInputShare::single_peer(edge, emergency_stop_keycode);
    loop {
        let event = if share.is_remote() && remote_focus_timeout_millis > 0 {
            match tokio::time::timeout(
                std::time::Duration::from_millis(remote_focus_timeout_millis),
                capture.next_event(),
            )
            .await
            {
                Ok(Ok(event)) => event,
                Ok(Err(error)) => {
                    if share.release_remote() {
                        set_suppressed(false);
                    }
                    return Err(error.into());
                }
                Err(_) => {
                    if share.release_remote() {
                        set_suppressed(false);
                    }
                    continue;
                }
            }
        } else {
            capture.next_event().await?
        };
        if share.is_remote()
            && matches!(event, InputEvent::KeyPress(keycode) if keycode == emergency_stop_keycode)
        {
            if share.release_remote() {
                set_suppressed(false);
            }
            return Err(InputSessionError::EmergencyStop);
        }
        let was_remote = share.is_remote();
        let routed = share.route(event);
        let is_remote = share.is_remote();
        if was_remote != is_remote {
            set_suppressed(is_remote);
        }
        if let Some(event) = routed {
            if let Err(error) = connection.send(encode_input_event(next_id, event)).await {
                if share.release_remote() {
                    set_suppressed(false);
                }
                return Err(error.into());
            }
            next_id = next_id.next();
        }
    }
}

fn at_handoff_edge(edge: HandoffEdge, x: f64, y: f64) -> bool {
    const EPSILON: f64 = 0.995;
    match edge {
        HandoffEdge::Left => x <= 1.0 - EPSILON,
        HandoffEdge::Right => x >= EPSILON,
        HandoffEdge::Top => y <= 1.0 - EPSILON,
        HandoffEdge::Bottom => y >= EPSILON,
    }
}

#[cfg(test)]
fn entry_for_edge(edge: HandoffEdge, x: f64, y: f64) -> (f64, f64) {
    match edge {
        HandoffEdge::Left => (1.0, y.clamp(0.0, 1.0)),
        HandoffEdge::Right => (0.0, y.clamp(0.0, 1.0)),
        HandoffEdge::Top => (x.clamp(0.0, 1.0), 1.0),
        HandoffEdge::Bottom => (x.clamp(0.0, 1.0), 0.0),
    }
}

#[cfg(test)]
fn returned_to_local(edge: HandoffEdge, x: f64, y: f64) -> bool {
    match edge {
        HandoffEdge::Left => x > 1.0,
        HandoffEdge::Right => x < 0.0,
        HandoffEdge::Top => y > 1.0,
        HandoffEdge::Bottom => y < 0.0,
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

pub fn plan_runtime(
    role: InputRuntimeRole,
    can_capture_input: bool,
    can_inject_input: bool,
) -> InputRuntimePlan {
    match role {
        InputRuntimeRole::Disabled => InputRuntimePlan {
            start_capture_forwarder: false,
            start_inject_receiver: false,
        },
        InputRuntimeRole::Source => InputRuntimePlan {
            start_capture_forwarder: can_capture_input,
            start_inject_receiver: false,
        },
        InputRuntimeRole::Target => InputRuntimePlan {
            start_capture_forwarder: false,
            start_inject_receiver: can_inject_input,
        },
        InputRuntimeRole::Both => InputRuntimePlan {
            start_capture_forwarder: can_capture_input,
            start_inject_receiver: can_inject_input,
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

    #[test]
    fn extended_share_stays_local_until_handoff_edge() {
        let mut share = ExtendedInputShare::new(HandoffEdge::Right, 41);

        assert_eq!(
            share.route(InputEvent::PointerMove { x: 0.5, y: 0.5 }),
            None
        );
        assert!(!share.is_remote());

        assert_eq!(
            share.route(InputEvent::KeyPress(0x04)),
            None,
            "keyboard stays local before remote focus"
        );
    }

    #[test]
    fn extended_share_enters_remote_at_configured_edge() {
        let mut share = ExtendedInputShare::new(HandoffEdge::Right, 41);

        assert_eq!(
            share.route(InputEvent::PointerMove { x: 1.0, y: 0.25 }),
            Some(InputEvent::PointerMove { x: 0.0, y: 0.25 })
        );
        assert!(share.is_remote());
    }

    #[test]
    fn extended_share_forwards_keyboard_and_remote_motion() {
        let mut share = ExtendedInputShare::new(HandoffEdge::Right, 41);
        assert!(
            share
                .route(InputEvent::PointerMove { x: 1.0, y: 0.5 })
                .is_some()
        );

        assert_eq!(
            share.route(InputEvent::KeyPress(0x04)),
            Some(InputEvent::KeyPress(0x04))
        );
        assert_eq!(
            share.route(InputEvent::RelativeMove { dx: 0.25, dy: 0.1 }),
            Some(InputEvent::PointerMove { x: 0.25, y: 0.6 })
        );
    }

    #[test]
    fn extended_share_returns_local_when_remote_crosses_back() {
        let mut share = ExtendedInputShare::new(HandoffEdge::Right, 41);
        assert!(
            share
                .route(InputEvent::PointerMove { x: 1.0, y: 0.5 })
                .is_some()
        );

        assert_eq!(
            share.route(InputEvent::RelativeMove { dx: -0.1, dy: 0.0 }),
            None
        );
        assert!(!share.is_remote());
    }

    #[test]
    fn emergency_key_returns_remote_focus_to_local_without_forwarding() {
        let mut share = ExtendedInputShare::new(HandoffEdge::Right, 41);
        assert!(
            share
                .route(InputEvent::PointerMove { x: 1.0, y: 0.5 })
                .is_some()
        );
        assert!(share.is_remote());

        assert_eq!(share.route(InputEvent::KeyPress(41)), None);
        assert!(!share.is_remote());
    }

    #[test]
    fn release_remote_is_idempotent_and_returns_to_local() {
        let mut share = ExtendedInputShare::new(HandoffEdge::Right, 41);
        assert!(!share.release_remote());
        assert!(
            share
                .route(InputEvent::PointerMove { x: 1.0, y: 0.5 })
                .is_some()
        );
        assert!(share.release_remote());
        assert!(!share.is_remote());
        assert!(!share.release_remote());
    }

    #[test]
    fn linked_screen_share_uses_controller_for_entry_motion_and_return() {
        let mut share = LinkedScreenInputShare::single_peer(HandoffEdge::Right, 41);

        assert_eq!(
            share.route(InputEvent::PointerMove { x: 0.5, y: 0.5 }),
            None
        );

        assert_eq!(
            share.route(InputEvent::PointerMove { x: 1.0, y: 0.25 }),
            Some(InputEvent::PointerMove { x: 0.0, y: 0.25 })
        );
        assert!(share.is_remote());

        assert_eq!(
            share.route(InputEvent::RelativeMove { dx: 0.2, dy: 0.1 }),
            Some(InputEvent::PointerMove { x: 0.2, y: 0.35 })
        );

        assert_eq!(
            share.route(InputEvent::RelativeMove { dx: -0.3, dy: 0.0 }),
            None
        );
        assert!(!share.is_remote());
    }

    #[test]
    fn linked_screen_share_hands_off_from_a_clamped_last_display_pixel() {
        let mut share = LinkedScreenInputShare::single_peer(HandoffEdge::Right, 41);
        let last_pixel_x = 1727.0 / 1728.0;

        assert_eq!(
            share.route(InputEvent::PointerMove {
                x: last_pixel_x,
                y: 0.5,
            }),
            Some(InputEvent::PointerMove { x: 0.0, y: 0.5 })
        );
        assert!(share.is_remote());
    }

    #[test]
    fn linked_screen_share_hands_off_at_each_clamped_edge() {
        let cases = [
            (
                HandoffEdge::Left,
                InputEvent::PointerMove { x: 0.0, y: 0.25 },
                InputEvent::PointerMove { x: 1.0, y: 0.25 },
            ),
            (
                HandoffEdge::Right,
                InputEvent::PointerMove {
                    x: 1727.0 / 1728.0,
                    y: 0.25,
                },
                InputEvent::PointerMove { x: 0.0, y: 0.25 },
            ),
            (
                HandoffEdge::Top,
                InputEvent::PointerMove { x: 0.25, y: 0.0 },
                InputEvent::PointerMove { x: 0.25, y: 1.0 },
            ),
            (
                HandoffEdge::Bottom,
                InputEvent::PointerMove {
                    x: 0.25,
                    y: 1116.0 / 1117.0,
                },
                InputEvent::PointerMove { x: 0.25, y: 0.0 },
            ),
        ];

        for (edge, event, expected) in cases {
            let mut share = LinkedScreenInputShare::single_peer(edge, 41);
            assert_eq!(share.route(event), Some(expected), "edge: {edge:?}");
            assert!(share.is_remote(), "edge: {edge:?}");
        }
    }

    #[test]
    fn linked_screen_share_forwards_clicks_while_remote() {
        let mut share = LinkedScreenInputShare::single_peer(HandoffEdge::Right, 41);
        assert!(
            share
                .route(InputEvent::PointerMove { x: 1.0, y: 0.5 })
                .is_some()
        );

        assert_eq!(
            share.route(InputEvent::ButtonPress(nexkvm_input::MouseButton::Left)),
            Some(InputEvent::ButtonPress(nexkvm_input::MouseButton::Left))
        );
        assert_eq!(
            share.route(InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left)),
            Some(InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left))
        );
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
    struct TimeoutThenErrorCapture {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl InputCapture for TimeoutThenErrorCapture {
        async fn next_event(&self) -> Result<InputEvent, InputError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match call {
                0 => Ok(InputEvent::PointerMove { x: 1.0, y: 0.5 }),
                1 => {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    Err(InputError::Backend("capture delayed error".into()))
                }
                _ => Err(InputError::Backend("capture finished".into())),
            }
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

    #[derive(Debug, Default)]
    struct FailingSendConnection {
        sent: Mutex<Vec<Envelope>>,
    }

    #[async_trait]
    impl Connection for FailingSendConnection {
        fn kind(&self) -> TransportKind {
            TransportKind::Tcp
        }

        fn peer_addr(&self) -> SocketAddr {
            "127.0.0.1:47654".parse().unwrap()
        }

        async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
            self.sent.lock().unwrap().push(envelope);
            Err(NetworkError::Closed)
        }

        async fn recv(&self) -> Result<Envelope, NetworkError> {
            Err(NetworkError::Closed)
        }

        async fn close(&self) -> Result<(), NetworkError> {
            Ok(())
        }
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
    async fn local_emergency_key_before_handoff_does_not_stop_forwarding() {
        let capture = QueueCapture::new(vec![InputEvent::KeyPress(41)]);
        let connection = Arc::new(MemoryConnection::default());
        let suppressions = Arc::new(Mutex::new(Vec::new()));
        let suppressions_for_callback = Arc::clone(&suppressions);

        let error = forward_extended_until_error(
            &capture,
            &*connection,
            MessageId(30),
            HandoffEdge::Right,
            41,
            3_000,
            move |suppressed| suppressions_for_callback.lock().unwrap().push(suppressed),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, InputSessionError::Codec(_)));
        assert!(connection.sent.lock().unwrap().is_empty());
        assert!(suppressions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn emergency_key_stops_remote_forwarding_without_sending_key() {
        let capture = QueueCapture::new(vec![
            InputEvent::PointerMove { x: 1.0, y: 0.5 },
            InputEvent::KeyPress(41),
        ]);
        let connection = Arc::new(MemoryConnection::default());
        let suppressions = Arc::new(Mutex::new(Vec::new()));
        let suppressions_for_callback = Arc::clone(&suppressions);

        let error = forward_extended_until_error(
            &capture,
            &*connection,
            MessageId(30),
            HandoffEdge::Right,
            41,
            3_000,
            move |suppressed| suppressions_for_callback.lock().unwrap().push(suppressed),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, InputSessionError::EmergencyStop));
        let sent = connection.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            decode_input_event(sent[0].clone()).unwrap(),
            InputEvent::PointerMove { x: 0.0, y: 0.5 }
        );
        assert_eq!(suppressions.lock().unwrap().as_slice(), &[true, false]);
    }

    #[tokio::test]
    async fn send_failure_releases_remote_suppression() {
        let capture = QueueCapture::new(vec![InputEvent::PointerMove { x: 1.0, y: 0.5 }]);
        let connection = Arc::new(FailingSendConnection::default());
        let suppressions = Arc::new(Mutex::new(Vec::new()));
        let suppressions_for_callback = Arc::clone(&suppressions);

        let error = forward_extended_until_error(
            &capture,
            &*connection,
            MessageId(70),
            HandoffEdge::Right,
            41,
            3_000,
            move |suppressed| suppressions_for_callback.lock().unwrap().push(suppressed),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, InputSessionError::Codec(_)));
        assert_eq!(connection.sent.lock().unwrap().len(), 1);
        assert_eq!(suppressions.lock().unwrap().as_slice(), &[true, false]);
    }

    #[tokio::test]
    async fn forward_extended_sends_clicks_after_remote_handoff() {
        let capture = QueueCapture::new(vec![
            InputEvent::PointerMove { x: 1.0, y: 0.5 },
            InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
            InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
        ]);
        let connection = Arc::new(MemoryConnection::default());

        let error = forward_extended_until_error(
            &capture,
            &*connection,
            MessageId(40),
            HandoffEdge::Right,
            41,
            3_000,
            |_| {},
        )
        .await
        .unwrap_err();

        assert!(matches!(error, InputSessionError::Codec(_)));
        let sent = connection.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 3);
        assert_eq!(
            decode_input_event(sent[1].clone()).unwrap(),
            InputEvent::ButtonPress(nexkvm_input::MouseButton::Left)
        );
        assert_eq!(
            decode_input_event(sent[2].clone()).unwrap(),
            InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left)
        );
    }

    #[tokio::test]
    async fn forward_extended_toggles_suppression_on_handoff_and_timeout_release() {
        let capture = TimeoutThenErrorCapture::default();
        let connection = Arc::new(MemoryConnection::default());
        let suppressions = Arc::new(Mutex::new(Vec::new()));
        let suppressions_for_callback = Arc::clone(&suppressions);

        let error = forward_extended_until_error(
            &capture,
            &*connection,
            MessageId(50),
            HandoffEdge::Right,
            41,
            5,
            move |suppressed| suppressions_for_callback.lock().unwrap().push(suppressed),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, InputSessionError::Codec(_)));
        assert_eq!(suppressions.lock().unwrap().as_slice(), &[true, false]);
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
    fn target_role_starts_receiver_only_when_inject_is_ready() {
        assert_eq!(
            plan_runtime(InputRuntimeRole::Target, false, true),
            InputRuntimePlan {
                start_capture_forwarder: false,
                start_inject_receiver: true,
            }
        );
        assert_eq!(
            plan_runtime(InputRuntimeRole::Target, true, false),
            InputRuntimePlan {
                start_capture_forwarder: false,
                start_inject_receiver: false,
            }
        );
    }

    #[test]
    fn source_role_starts_capture_only_when_capture_is_ready() {
        assert_eq!(
            plan_runtime(InputRuntimeRole::Source, true, false),
            InputRuntimePlan {
                start_capture_forwarder: true,
                start_inject_receiver: false,
            }
        );
        assert_eq!(
            plan_runtime(InputRuntimeRole::Source, false, true),
            InputRuntimePlan {
                start_capture_forwarder: false,
                start_inject_receiver: false,
            }
        );
    }

    #[test]
    fn both_role_enables_each_direction_independently() {
        assert_eq!(
            plan_runtime(InputRuntimeRole::Both, true, false),
            InputRuntimePlan {
                start_capture_forwarder: true,
                start_inject_receiver: false,
            }
        );
        assert_eq!(
            plan_runtime(InputRuntimeRole::Both, false, true),
            InputRuntimePlan {
                start_capture_forwarder: false,
                start_inject_receiver: true,
            }
        );
    }
}
