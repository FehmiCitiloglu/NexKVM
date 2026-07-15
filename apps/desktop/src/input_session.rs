use bytes::Bytes;
use nexkvm_core::identity::DeviceId;
use nexkvm_input::{
    BoundaryDetector, DisplayRect, Edge, EdgeLink, InputCapture, InputError, InputEvent,
    InputInjector, MonitorId, MonitorLayout, MouseShareController, ShareOutput,
};
use nexkvm_network::{Connection, NetworkError};
use nexkvm_protocol::{Envelope, MessageId, MessageKind, PROTOCOL_VERSION};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const INPUT_CLEANUP_STEP_TIMEOUT: Duration = Duration::from_millis(500);

/// Tracks every live input task so daemon shutdown can request cleanup and
/// wait for held-input releases instead of detaching platform work.
#[derive(Debug, Clone)]
pub(crate) struct InputTaskSupervisor {
    shutdown: tokio::sync::watch::Sender<bool>,
    state: Arc<std::sync::Mutex<InputTaskSupervisorState>>,
}

#[derive(Debug)]
struct InputTaskSupervisorState {
    accepting: bool,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl InputTaskSupervisor {
    pub(crate) fn new() -> Self {
        let (shutdown, _) = tokio::sync::watch::channel(false);
        Self {
            shutdown,
            state: Arc::new(std::sync::Mutex::new(InputTaskSupervisorState {
                accepting: true,
                tasks: Vec::new(),
            })),
        }
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub(crate) fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return;
        }
        state.tasks.retain(|task| !task.is_finished());
        state.tasks.push(tokio::spawn(future));
    }

    /// Returns `true` when every registered task completed before `timeout`.
    /// Timed-out tasks are aborted only after they were given the full cleanup
    /// window; the input loops themselves bound individual I/O cleanup steps.
    pub(crate) async fn shutdown(&self, timeout: Duration) -> bool {
        self.shutdown.send_replace(true);
        let now = Instant::now();
        let deadline = now.checked_add(timeout).unwrap_or(now);
        let mut clean = true;
        loop {
            let mut tasks = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.tasks.is_empty() {
                    state.accepting = false;
                    return clean;
                }
                std::mem::take(&mut state.tasks)
            };
            while let Some(mut task) = tasks.pop() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    task.abort();
                } else {
                    match tokio::time::timeout(remaining, &mut task).await {
                        Ok(Ok(())) => continue,
                        Ok(Err(error)) if error.is_cancelled() => {
                            clean = false;
                            continue;
                        }
                        Ok(Err(error)) => {
                            tracing::warn!(%error, "input task failed during shutdown");
                            clean = false;
                            continue;
                        }
                        Err(_) => {
                            task.abort();
                        }
                    }
                }
                let _ = task.await;
                for task in tasks.drain(..) {
                    task.abort();
                }
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.accepting = false;
                for task in state.tasks.drain(..) {
                    task.abort();
                }
                return false;
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InputSessionError {
    #[error("input payload codec error: {0}")]
    Codec(String),
    #[error("unexpected message kind: {0:?}")]
    UnexpectedKind(MessageKind),
}

/// Prevents duplicate peer connections from racing to consume the single
/// platform input stream. The lease is released when its forwarding task ends.
#[derive(Debug, Default)]
pub(crate) struct InputForwarderGate {
    active: AtomicBool,
}

impl InputForwarderGate {
    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<InputForwarderLease> {
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| InputForwarderLease {
                gate: Arc::clone(self),
            })
    }
}

#[derive(Debug)]
pub(crate) struct InputForwarderLease {
    gate: Arc<InputForwarderGate>,
}

impl Drop for InputForwarderLease {
    fn drop(&mut self) {
        self.gate.active.store(false, Ordering::Release);
    }
}

#[allow(dead_code)]
pub fn encode_input_event(id: MessageId, event: InputEvent) -> Result<Envelope, InputSessionError> {
    validate_input_event(event)?;
    let body =
        serde_json::to_vec(&event).map_err(|error| InputSessionError::Codec(error.to_string()))?;
    Ok(Envelope::new(
        PROTOCOL_VERSION,
        id,
        MessageKind::Input,
        Bytes::from(body),
    ))
}

pub fn decode_input_event(envelope: Envelope) -> Result<InputEvent, InputSessionError> {
    if envelope.kind != MessageKind::Input {
        return Err(InputSessionError::UnexpectedKind(envelope.kind));
    }
    let event: InputEvent = serde_json::from_slice(&envelope.body)
        .map_err(|error| InputSessionError::Codec(error.to_string()))?;
    validate_input_event(event)?;
    Ok(event)
}

fn validate_input_event(event: InputEvent) -> Result<(), InputSessionError> {
    let valid = match event {
        InputEvent::PointerMove { x, y } => {
            x.is_finite() && y.is_finite() && (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)
        }
        InputEvent::RelativeMove { dx, dy } => {
            dx.is_finite() && dy.is_finite() && dx.abs() <= 4.0 && dy.abs() <= 4.0
        }
        InputEvent::RawMotion { dx, dy } => {
            dx.unsigned_abs() <= 100_000 && dy.unsigned_abs() <= 100_000
        }
        InputEvent::Scroll { dx, dy } => {
            dx.is_finite() && dy.is_finite() && dx.abs() <= 10_000.0 && dy.abs() <= 10_000.0
        }
        InputEvent::KeyPress(keycode) | InputEvent::KeyRelease(keycode) => {
            keycode <= u16::MAX.into()
        }
        InputEvent::ButtonPress(_) | InputEvent::ButtonRelease(_) => true,
    };
    if valid {
        Ok(())
    } else {
        Err(InputSessionError::Codec(
            "input event contains an invalid or out-of-range value".into(),
        ))
    }
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
        connection.send(encode_input_event(next_id, event)?).await?;
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
        connection.send(encode_input_event(next_id, event)?).await?;
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
    held_remote_inputs: Vec<HeldRemoteInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeldRemoteInput {
    Key(u32),
    Button(nexkvm_input::MouseButton),
}

impl HeldRemoteInput {
    fn release_event(self) -> InputEvent {
        match self {
            Self::Key(keycode) => InputEvent::KeyRelease(keycode),
            Self::Button(button) => InputEvent::ButtonRelease(button),
        }
    }
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
            held_remote_inputs: Vec::new(),
        }
    }

    pub fn is_remote(&self) -> bool {
        self.controller.active_peer().is_some()
    }

    pub fn release_remote(&mut self) -> bool {
        let was_remote = self.is_remote();
        let _ = self.release_remote_events();
        was_remote
    }

    fn reconfigure_edge(&mut self, edge: HandoffEdge) -> Vec<InputEvent> {
        if self.edge == edge {
            return Vec::new();
        }
        let releases = self.release_remote_events();
        *self = Self::single_peer(edge, self.emergency_stop_keycode);
        releases
    }

    #[allow(dead_code)]
    pub fn route(&mut self, event: InputEvent) -> Option<InputEvent> {
        self.route_events(event).into_iter().next()
    }

    fn route_events(&mut self, event: InputEvent) -> Vec<InputEvent> {
        if matches!(event, InputEvent::KeyPress(keycode) if keycode == self.emergency_stop_keycode)
        {
            return self.release_remote_events();
        }

        if !self.is_remote() {
            return self.route_local(event).into_iter().collect();
        }

        self.route_remote_events(event)
    }

    fn route_local(&mut self, event: InputEvent) -> Option<InputEvent> {
        let InputEvent::PointerMove { x, y } = event else {
            return None;
        };
        self.last_local_pos = Some((x, y));
        let (px, py) = boundary_sample_for_edge(self.edge, x, y);
        match self.controller.on_local_cursor(px, py) {
            ShareOutput::EnterRemote(entry) => {
                self.held_remote_inputs.clear();
                Some(entry.entry_event())
            }
            _ => None,
        }
    }

    fn route_remote_events(&mut self, event: InputEvent) -> Vec<InputEvent> {
        match event {
            InputEvent::RelativeMove { dx, dy } => self.route_remote_motion(dx, dy),
            InputEvent::PointerMove { x, y } => {
                let (last_x, last_y) = self.last_local_pos.unwrap_or((x, y));
                self.last_local_pos = Some((x, y));
                self.route_remote_motion(x - last_x, y - last_y)
            }
            other => {
                self.track_remote_input(other);
                vec![other]
            }
        }
    }

    fn route_remote_motion(&mut self, dx: f64, dy: f64) -> Vec<InputEvent> {
        match self.controller.on_remote_motion(dx, dy) {
            ShareOutput::Forward { event, .. } => vec![event],
            ShareOutput::ReturnLocal { .. } => {
                self.last_local_pos = None;
                self.drain_held_remote_releases()
            }
            ShareOutput::Idle | ShareOutput::EnterRemote(_) => Vec::new(),
        }
    }

    fn release_remote_events(&mut self) -> Vec<InputEvent> {
        let was_remote = self.controller.release_remote();
        self.last_local_pos = None;
        if was_remote {
            self.drain_held_remote_releases()
        } else {
            self.held_remote_inputs.clear();
            Vec::new()
        }
    }

    fn track_remote_input(&mut self, event: InputEvent) {
        track_held_input(&mut self.held_remote_inputs, event);
    }

    fn drain_held_remote_releases(&mut self) -> Vec<InputEvent> {
        self.held_remote_inputs
            .drain(..)
            .rev()
            .map(HeldRemoteInput::release_event)
            .collect()
    }
}

fn track_held_input(held_inputs: &mut Vec<HeldRemoteInput>, event: InputEvent) {
    let (input, pressed) = match event {
        InputEvent::KeyPress(keycode) => (HeldRemoteInput::Key(keycode), true),
        InputEvent::KeyRelease(keycode) => (HeldRemoteInput::Key(keycode), false),
        InputEvent::ButtonPress(button) => (HeldRemoteInput::Button(button), true),
        InputEvent::ButtonRelease(button) => (HeldRemoteInput::Button(button), false),
        _ => return,
    };

    if pressed {
        if !held_inputs.contains(&input) {
            held_inputs.push(input);
        }
    } else if let Some(index) = held_inputs.iter().rposition(|held| *held == input) {
        held_inputs.remove(index);
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

async fn send_input_events<K>(
    connection: &K,
    next_id: &mut MessageId,
    events: Vec<InputEvent>,
) -> Result<(), InputSessionError>
where
    K: Connection + ?Sized,
{
    for event in events {
        let envelope = encode_input_event(*next_id, event)?;
        match tokio::time::timeout(INPUT_CLEANUP_STEP_TIMEOUT, connection.send(envelope)).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(InputSessionError::Codec(
                    "timed out sending input event; session closed for input safety".into(),
                ));
            }
        }
        *next_id = next_id.next();
    }
    Ok(())
}

async fn send_cleanup_input_events<K>(
    connection: &K,
    next_id: &mut MessageId,
    events: Vec<InputEvent>,
    reason: &'static str,
) where
    K: Connection + ?Sized,
{
    match tokio::time::timeout(
        INPUT_CLEANUP_STEP_TIMEOUT,
        send_input_events(connection, next_id, events),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, reason, "failed to release held remote inputs"),
        Err(_) => tracing::warn!(reason, "timed out releasing held remote inputs"),
    }
}

#[allow(dead_code)]
pub async fn forward_extended_until_error<C, K, S>(
    capture: &C,
    connection: &K,
    first_id: MessageId,
    edge: HandoffEdge,
    emergency_stop_keycode: u32,
    remote_focus_timeout_millis: u64,
    set_suppressed: S,
) -> Result<(), InputSessionError>
where
    C: InputCapture + ?Sized,
    K: Connection + ?Sized,
    S: FnMut(bool),
{
    let (edge_sender, edge_receiver) = tokio::sync::watch::channel(edge);
    let result = forward_reconfigurable_until_error(
        capture,
        connection,
        first_id,
        edge_receiver,
        emergency_stop_keycode,
        remote_focus_timeout_millis,
        set_suppressed,
    )
    .await;
    drop(edge_sender);
    result
}

enum ForwardWait {
    Event(Result<InputEvent, InputError>),
    FocusTimeout,
    Topology(Result<(), tokio::sync::watch::error::RecvError>),
    Shutdown(Result<(), tokio::sync::watch::error::RecvError>),
}

struct SuppressionGuard<S>
where
    S: FnMut(bool),
{
    setter: S,
    suppressed: bool,
}

impl<S> SuppressionGuard<S>
where
    S: FnMut(bool),
{
    fn new(setter: S) -> Self {
        Self {
            setter,
            suppressed: false,
        }
    }

    fn set(&mut self, suppressed: bool) {
        if self.suppressed != suppressed {
            (self.setter)(suppressed);
            self.suppressed = suppressed;
        }
    }

    fn restore(&mut self) {
        (self.setter)(false);
        self.suppressed = false;
    }
}

impl<S> Drop for SuppressionGuard<S>
where
    S: FnMut(bool),
{
    fn drop(&mut self) {
        if self.suppressed {
            (self.setter)(false);
            self.suppressed = false;
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InputForwardingConfig {
    pub(crate) emergency_stop_keycode: u32,
    pub(crate) remote_focus_timeout_millis: u64,
}

pub async fn forward_reconfigurable_until_error<C, K, S>(
    capture: &C,
    connection: &K,
    first_id: MessageId,
    topology: tokio::sync::watch::Receiver<HandoffEdge>,
    emergency_stop_keycode: u32,
    remote_focus_timeout_millis: u64,
    set_suppressed: S,
) -> Result<(), InputSessionError>
where
    C: InputCapture + ?Sized,
    K: Connection + ?Sized,
    S: FnMut(bool),
{
    let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
    forward_reconfigurable_until_shutdown(
        capture,
        connection,
        first_id,
        topology,
        shutdown,
        InputForwardingConfig {
            emergency_stop_keycode,
            remote_focus_timeout_millis,
        },
        set_suppressed,
    )
    .await
}

pub async fn forward_reconfigurable_until_shutdown<C, K, S>(
    capture: &C,
    connection: &K,
    first_id: MessageId,
    mut topology: tokio::sync::watch::Receiver<HandoffEdge>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    config: InputForwardingConfig,
    set_suppressed: S,
) -> Result<(), InputSessionError>
where
    C: InputCapture + ?Sized,
    K: Connection + ?Sized,
    S: FnMut(bool),
{
    let mut next_id = first_id;
    let initial_edge = *topology.borrow();
    let mut share =
        LinkedScreenInputShare::single_peer(initial_edge, config.emergency_stop_keycode);
    let mut suppression = SuppressionGuard::new(set_suppressed);
    let mut topology_open = true;
    let mut shutdown_open = true;
    loop {
        let next = if *shutdown.borrow() {
            ForwardWait::Shutdown(Ok(()))
        } else if share.is_remote() && config.remote_focus_timeout_millis > 0 {
            tokio::select! {
                biased;
                changed = shutdown.changed(), if shutdown_open => ForwardWait::Shutdown(changed),
                changed = topology.changed(), if topology_open => ForwardWait::Topology(changed),
                event = capture.next_event() => ForwardWait::Event(event),
                () = tokio::time::sleep(std::time::Duration::from_millis(
                    config.remote_focus_timeout_millis,
                )) => ForwardWait::FocusTimeout,
            }
        } else {
            tokio::select! {
                biased;
                changed = shutdown.changed(), if shutdown_open => ForwardWait::Shutdown(changed),
                changed = topology.changed(), if topology_open => ForwardWait::Topology(changed),
                event = capture.next_event() => ForwardWait::Event(event),
            }
        };

        let event = match next {
            ForwardWait::Shutdown(Ok(())) if !*shutdown.borrow_and_update() => continue,
            ForwardWait::Shutdown(Ok(())) => {
                let releases = share.release_remote_events();
                send_cleanup_input_events(connection, &mut next_id, releases, "shutdown").await;
                suppression.restore();
                close_input_connection(connection).await;
                return Ok(());
            }
            ForwardWait::Shutdown(Err(_)) => {
                shutdown_open = false;
                continue;
            }
            ForwardWait::Topology(Ok(())) => {
                let next_edge = *topology.borrow_and_update();
                if next_edge == share.edge {
                    continue;
                }
                let was_remote = share.is_remote();
                let releases = share.reconfigure_edge(next_edge);
                if let Err(error) = send_input_events(connection, &mut next_id, releases).await {
                    if was_remote {
                        suppression.set(false);
                    }
                    close_input_connection(connection).await;
                    return Err(error);
                }
                if was_remote {
                    suppression.set(false);
                }
                continue;
            }
            ForwardWait::Topology(Err(_)) => {
                topology_open = false;
                continue;
            }
            ForwardWait::FocusTimeout => {
                let was_remote = share.is_remote();
                let releases = share.release_remote_events();
                if let Err(error) = send_input_events(connection, &mut next_id, releases).await {
                    suppression.set(false);
                    close_input_connection(connection).await;
                    return Err(error);
                }
                if was_remote {
                    suppression.set(false);
                }
                continue;
            }
            ForwardWait::Event(Ok(event)) => event,
            ForwardWait::Event(Err(error)) => {
                let was_remote = share.is_remote();
                let releases = share.release_remote_events();
                send_cleanup_input_events(connection, &mut next_id, releases, "capture error")
                    .await;
                if was_remote {
                    suppression.set(false);
                }
                close_input_connection(connection).await;
                return Err(error.into());
            }
        };
        let was_remote = share.is_remote();
        let routed = share.route_events(event);
        let is_remote = share.is_remote();
        if !was_remote && is_remote {
            suppression.set(true);
        }
        if let Err(error) = send_input_events(connection, &mut next_id, routed).await {
            if share.release_remote() || was_remote {
                suppression.set(false);
            }
            close_input_connection(connection).await;
            return Err(error);
        }
        if was_remote && !is_remote {
            suppression.set(false);
        }
    }
}

pub(crate) async fn close_input_connection<K>(connection: &K)
where
    K: Connection + ?Sized,
{
    match tokio::time::timeout(INPUT_CLEANUP_STEP_TIMEOUT, connection.close()).await {
        Ok(Ok(())) | Ok(Err(NetworkError::Closed)) => {}
        Ok(Err(error)) => tracing::warn!(%error, "failed to close input connection"),
        Err(_) => tracing::warn!("timed out closing input connection"),
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

#[allow(dead_code)]
pub async fn inject_until_closed<K, I>(
    connection: &K,
    injector: &I,
) -> Result<(), InputSessionError>
where
    K: Connection + ?Sized,
    I: InputInjector + ?Sized,
{
    let (_shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
    inject_until_shutdown(connection, injector, shutdown).await
}

pub async fn inject_until_shutdown<K, I>(
    connection: &K,
    injector: &I,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), InputSessionError>
where
    K: Connection + ?Sized,
    I: InputInjector + ?Sized,
{
    let mut held_inputs = Vec::new();
    let mut shutdown_requested = *shutdown.borrow();
    let mut shutdown_open = true;
    let terminal = loop {
        enum InjectWait {
            Envelope(Result<Envelope, NetworkError>),
            Shutdown(Result<(), tokio::sync::watch::error::RecvError>),
        }
        let next = if *shutdown.borrow() {
            InjectWait::Shutdown(Ok(()))
        } else {
            tokio::select! {
                biased;
                changed = shutdown.changed(), if shutdown_open => InjectWait::Shutdown(changed),
                envelope = connection.recv() => InjectWait::Envelope(envelope),
            }
        };
        let received = match next {
            InjectWait::Shutdown(Ok(())) if !*shutdown.borrow_and_update() => continue,
            InjectWait::Shutdown(Ok(())) => {
                shutdown_requested = true;
                break Ok(());
            }
            InjectWait::Shutdown(Err(_)) => {
                shutdown_open = false;
                continue;
            }
            InjectWait::Envelope(received) => received,
        };
        match received {
            Ok(envelope) => {
                if envelope.kind != MessageKind::Input {
                    continue;
                }
                let event = match decode_input_event(envelope) {
                    Ok(event) => event,
                    Err(error) => break Err(error),
                };
                match injector.inject(event).await {
                    Ok(()) => track_held_input(&mut held_inputs, event),
                    Err(InputError::PermissionDenied) => {
                        break Err(InputError::PermissionDenied.into());
                    }
                    Err(error) => {
                        tracing::warn!(%error, "input event injection failed; continuing session");
                    }
                }
            }
            Err(NetworkError::Closed) => break Ok(()),
            Err(error) => break Err(error.into()),
        }
    };

    let mut release_error = None;
    for event in held_inputs
        .drain(..)
        .rev()
        .map(HeldRemoteInput::release_event)
    {
        let result = if shutdown_requested {
            match tokio::time::timeout(INPUT_CLEANUP_STEP_TIMEOUT, injector.inject(event)).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!("timed out releasing held input during shutdown");
                    if release_error.is_none() {
                        release_error = Some(InputSessionError::Codec(
                            "timed out releasing held input during shutdown".into(),
                        ));
                    }
                    continue;
                }
            }
        } else {
            injector.inject(event).await
        };
        if let Err(error) = result {
            tracing::warn!(%error, "failed to release held input after receiver ended");
            if release_error.is_none() {
                release_error = Some(error.into());
            }
        }
    }
    close_input_connection(connection).await;
    match (terminal, release_error) {
        (Err(error), _) => Err(error),
        (Ok(()), Some(error)) => Err(error),
        (Ok(()), None) => Ok(()),
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn input_forwarder_gate_allows_only_one_live_connection() {
        let gate = Arc::new(InputForwarderGate::default());

        let first = gate.try_acquire().expect("first connection owns capture");
        assert!(
            gate.try_acquire().is_none(),
            "duplicate connection is rejected"
        );

        drop(first);
        assert!(
            gate.try_acquire().is_some(),
            "capture is released after session end"
        );
    }

    #[test]
    fn suppression_guard_restores_local_cursor_when_forwarder_future_is_dropped() {
        let transitions = Arc::new(Mutex::new(Vec::new()));
        let transitions_for_setter = Arc::clone(&transitions);
        {
            let mut guard = SuppressionGuard::new(move |suppressed| {
                transitions_for_setter.lock().unwrap().push(suppressed);
            });
            guard.set(true);
        }

        assert_eq!(transitions.lock().unwrap().as_slice(), &[true, false]);
    }

    #[test]
    fn input_event_round_trips_through_envelope_body() {
        let event = InputEvent::KeyPress(0x04);
        let envelope = encode_input_event(MessageId(7), event).unwrap();

        assert_eq!(envelope.version, PROTOCOL_VERSION);
        assert_eq!(envelope.id, MessageId(7));
        assert_eq!(envelope.kind, MessageKind::Input);
        assert_eq!(decode_input_event(envelope).unwrap(), event);
    }

    #[test]
    fn invalid_outbound_input_is_rejected_before_serialization() {
        let result = encode_input_event(
            MessageId(7),
            InputEvent::PointerMove {
                x: f64::NAN,
                y: 0.5,
            },
        );

        assert!(matches!(result, Err(InputSessionError::Codec(_))));
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
    fn rejects_out_of_range_peer_input_before_injection() {
        let invalid_events = [
            br#"{"PointerMove":{"x":2.0,"y":0.5}}"#.as_slice(),
            br#"{"RelativeMove":{"dx":99.0,"dy":0.0}}"#.as_slice(),
            br#"{"RawMotion":{"dx":100001,"dy":0}}"#.as_slice(),
            br#"{"Scroll":{"dx":0.0,"dy":10001.0}}"#.as_slice(),
            br#"{"KeyPress":65536}"#.as_slice(),
        ];
        for body in invalid_events {
            let envelope = Envelope::new(
                PROTOCOL_VERSION,
                MessageId(1),
                MessageKind::Input,
                Bytes::copy_from_slice(body),
            );
            assert!(matches!(
                decode_input_event(envelope),
                Err(InputSessionError::Codec(_))
            ));
        }
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

    #[test]
    fn linked_screen_share_releases_held_inputs_when_returning_local() {
        let mut share = LinkedScreenInputShare::single_peer(HandoffEdge::Right, 41);
        assert_eq!(
            share.route_events(InputEvent::PointerMove { x: 1.0, y: 0.5 }),
            vec![InputEvent::PointerMove { x: 0.0, y: 0.5 }]
        );
        assert_eq!(
            share.route_events(InputEvent::KeyPress(0xE1)),
            vec![InputEvent::KeyPress(0xE1)]
        );
        assert_eq!(
            share.route_events(InputEvent::ButtonPress(nexkvm_input::MouseButton::Left)),
            vec![InputEvent::ButtonPress(nexkvm_input::MouseButton::Left)]
        );

        assert_eq!(
            share.route_events(InputEvent::RelativeMove { dx: -0.1, dy: 0.0 }),
            vec![
                InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
                InputEvent::KeyRelease(0xE1),
            ]
        );
        assert!(!share.is_remote());
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

    #[derive(Debug)]
    struct ChannelCapture {
        events: tokio::sync::Mutex<
            tokio::sync::mpsc::UnboundedReceiver<Result<InputEvent, InputError>>,
        >,
    }

    #[async_trait]
    impl InputCapture for ChannelCapture {
        async fn next_event(&self) -> Result<InputEvent, InputError> {
            self.events
                .lock()
                .await
                .recv()
                .await
                .unwrap_or_else(|| Err(InputError::Backend("capture channel closed".into())))
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
    struct HeldInputsThenTimeoutCapture {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl InputCapture for HeldInputsThenTimeoutCapture {
        async fn next_event(&self) -> Result<InputEvent, InputError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match call {
                0 => Ok(InputEvent::PointerMove { x: 1.0, y: 0.5 }),
                1 => Ok(InputEvent::KeyPress(0xE1)),
                2 => Ok(InputEvent::ButtonPress(nexkvm_input::MouseButton::Left)),
                3 => {
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

    #[derive(Debug, Default)]
    struct FailingOnceInjector {
        attempts: Mutex<Vec<InputEvent>>,
    }

    #[async_trait]
    impl InputInjector for RecordingInjector {
        async fn inject(&self, event: InputEvent) -> Result<(), InputError> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    #[async_trait]
    impl InputInjector for FailingOnceInjector {
        async fn inject(&self, event: InputEvent) -> Result<(), InputError> {
            let mut attempts = self.attempts.lock().unwrap();
            attempts.push(event);
            if attempts.len() == 1 {
                Err(InputError::Backend("synthetic injection failure".into()))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Debug, Default)]
    struct MemoryConnection {
        sent: Mutex<Vec<Envelope>>,
        recv: Mutex<VecDeque<Envelope>>,
        closed: AtomicUsize,
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
                closed: AtomicUsize::new(0),
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
            self.closed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Debug)]
    struct ChannelConnection {
        sent: Mutex<Vec<Envelope>>,
        recv: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Envelope>>,
        closed: AtomicUsize,
    }

    #[async_trait]
    impl Connection for ChannelConnection {
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
                .await
                .recv()
                .await
                .ok_or(NetworkError::Closed)
        }

        async fn close(&self) -> Result<(), NetworkError> {
            self.closed.fetch_add(1, Ordering::SeqCst);
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
    async fn emergency_key_returns_local_without_ending_the_forwarder() {
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

        assert!(matches!(error, InputSessionError::Codec(_)));
        let sent = connection.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            decode_input_event(sent[0].clone()).unwrap(),
            InputEvent::PointerMove { x: 0.0, y: 0.5 }
        );
        assert_eq!(suppressions.lock().unwrap().as_slice(), &[true, false]);
    }

    #[tokio::test]
    async fn emergency_release_sends_releases_for_held_remote_inputs() {
        let capture = QueueCapture::new(vec![
            InputEvent::PointerMove { x: 1.0, y: 0.5 },
            InputEvent::KeyPress(0xE1),
            InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
            InputEvent::KeyPress(41),
        ]);
        let connection = Arc::new(MemoryConnection::default());

        let error = forward_extended_until_error(
            &capture,
            &*connection,
            MessageId(80),
            HandoffEdge::Right,
            41,
            3_000,
            |_| {},
        )
        .await
        .unwrap_err();

        assert!(matches!(error, InputSessionError::Codec(_)));
        let sent = connection.sent.lock().unwrap().clone();
        let events: Vec<_> = sent
            .into_iter()
            .map(|envelope| decode_input_event(envelope).unwrap())
            .collect();
        assert_eq!(
            events,
            vec![
                InputEvent::PointerMove { x: 0.0, y: 0.5 },
                InputEvent::KeyPress(0xE1),
                InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
                InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
                InputEvent::KeyRelease(0xE1),
            ]
        );
    }

    #[tokio::test]
    async fn capture_error_sends_releases_for_held_remote_inputs() {
        let capture = QueueCapture::new(vec![
            InputEvent::PointerMove { x: 1.0, y: 0.5 },
            InputEvent::KeyPress(0xE1),
            InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
        ]);
        let connection = Arc::new(MemoryConnection::default());

        let error = forward_extended_until_error(
            &capture,
            &*connection,
            MessageId(85),
            HandoffEdge::Right,
            41,
            3_000,
            |_| {},
        )
        .await
        .unwrap_err();

        assert!(matches!(error, InputSessionError::Codec(_)));
        let sent = connection.sent.lock().unwrap().clone();
        let events: Vec<_> = sent
            .into_iter()
            .map(|envelope| decode_input_event(envelope).unwrap())
            .collect();
        assert_eq!(
            events,
            vec![
                InputEvent::PointerMove { x: 0.0, y: 0.5 },
                InputEvent::KeyPress(0xE1),
                InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
                InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
                InputEvent::KeyRelease(0xE1),
            ]
        );
    }

    #[tokio::test]
    async fn live_topology_reload_releases_remote_and_applies_the_new_edge() {
        let (event_sender, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let capture = ChannelCapture {
            events: tokio::sync::Mutex::new(event_receiver),
        };
        event_sender
            .send(Ok(InputEvent::PointerMove { x: 1.0, y: 0.5 }))
            .unwrap();
        event_sender.send(Ok(InputEvent::KeyPress(0xE1))).unwrap();
        event_sender
            .send(Ok(InputEvent::ButtonPress(nexkvm_input::MouseButton::Left)))
            .unwrap();

        let (topology_sender, topology_receiver) = tokio::sync::watch::channel(HandoffEdge::Right);
        let connection = Arc::new(MemoryConnection::default());
        let connection_for_driver = Arc::clone(&connection);
        let suppressions = Arc::new(Mutex::new(Vec::new()));
        let suppressions_for_callback = Arc::clone(&suppressions);

        let forward = forward_reconfigurable_until_error(
            &capture,
            &*connection,
            MessageId(95),
            topology_receiver,
            41,
            3_000,
            move |suppressed| suppressions_for_callback.lock().unwrap().push(suppressed),
        );
        let drive = async move {
            wait_for_sent_events(&connection_for_driver, 3).await;
            topology_sender.send(HandoffEdge::Left).unwrap();
            wait_for_sent_events(&connection_for_driver, 5).await;
            event_sender
                .send(Ok(InputEvent::PointerMove { x: 0.0, y: 0.5 }))
                .unwrap();
            wait_for_sent_events(&connection_for_driver, 6).await;
            drop(event_sender);
        };

        let (error, ()) = tokio::join!(forward, drive);
        assert!(matches!(error.unwrap_err(), InputSessionError::Codec(_)));
        let events: Vec<_> = connection
            .sent
            .lock()
            .unwrap()
            .clone()
            .into_iter()
            .map(|envelope| decode_input_event(envelope).unwrap())
            .collect();
        assert_eq!(
            events,
            vec![
                InputEvent::PointerMove { x: 0.0, y: 0.5 },
                InputEvent::KeyPress(0xE1),
                InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
                InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
                InputEvent::KeyRelease(0xE1),
                InputEvent::PointerMove { x: 1.0, y: 0.5 },
            ]
        );
        assert_eq!(
            suppressions.lock().unwrap().as_slice(),
            &[true, false, true, false]
        );
    }

    async fn wait_for_sent_events(connection: &MemoryConnection, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if connection.sent.lock().unwrap().len() >= expected {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("forwarder did not send expected events");
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
    async fn timeout_release_sends_releases_for_held_remote_inputs() {
        let capture = HeldInputsThenTimeoutCapture::default();
        let connection = Arc::new(MemoryConnection::default());

        let error = forward_extended_until_error(
            &capture,
            &*connection,
            MessageId(90),
            HandoffEdge::Right,
            41,
            5,
            |_| {},
        )
        .await
        .unwrap_err();

        assert!(matches!(error, InputSessionError::Codec(_)));
        let sent = connection.sent.lock().unwrap().clone();
        let events: Vec<_> = sent
            .into_iter()
            .map(|envelope| decode_input_event(envelope).unwrap())
            .collect();
        assert_eq!(
            events,
            vec![
                InputEvent::PointerMove { x: 0.0, y: 0.5 },
                InputEvent::KeyPress(0xE1),
                InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
                InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
                InputEvent::KeyRelease(0xE1),
            ]
        );
    }

    #[tokio::test]
    async fn injects_received_input_envelopes() {
        let injector = RecordingInjector::default();
        let connection = MemoryConnection::with_recv(vec![
            encode_input_event(
                MessageId(1),
                InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
            )
            .unwrap(),
            encode_input_event(
                MessageId(2),
                InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
            )
            .unwrap(),
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

    #[tokio::test]
    async fn receiver_disconnect_releases_injected_held_inputs_in_reverse_order() {
        let injector = RecordingInjector::default();
        let connection = MemoryConnection::with_recv(vec![
            encode_input_event(MessageId(1), InputEvent::KeyPress(0xE1)).unwrap(),
            encode_input_event(
                MessageId(2),
                InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
            )
            .unwrap(),
        ]);

        inject_until_closed(&connection, &injector).await.unwrap();

        assert_eq!(
            injector.events.lock().unwrap().as_slice(),
            &[
                InputEvent::KeyPress(0xE1),
                InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
                InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
                InputEvent::KeyRelease(0xE1),
            ]
        );
    }

    #[tokio::test]
    async fn injection_failure_does_not_end_the_receiver_session() {
        let injector = FailingOnceInjector::default();
        let connection = MemoryConnection::with_recv(vec![
            encode_input_event(MessageId(1), InputEvent::KeyPress(0x04)).unwrap(),
            encode_input_event(MessageId(2), InputEvent::KeyRelease(0x04)).unwrap(),
        ]);

        inject_until_closed(&connection, &injector).await.unwrap();

        assert_eq!(
            injector.attempts.lock().unwrap().as_slice(),
            &[InputEvent::KeyPress(0x04), InputEvent::KeyRelease(0x04)]
        );
    }

    #[tokio::test]
    async fn forwarder_shutdown_releases_remote_inputs_unsuppresses_and_closes() {
        let (event_sender, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let capture = ChannelCapture {
            events: tokio::sync::Mutex::new(event_receiver),
        };
        event_sender
            .send(Ok(InputEvent::PointerMove { x: 1.0, y: 0.5 }))
            .unwrap();
        event_sender.send(Ok(InputEvent::KeyPress(0xE1))).unwrap();
        let (_topology_sender, topology) = tokio::sync::watch::channel(HandoffEdge::Right);
        let (shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let connection = Arc::new(MemoryConnection::default());
        let connection_for_driver = Arc::clone(&connection);
        let suppressions = Arc::new(Mutex::new(Vec::new()));
        let suppressions_for_callback = Arc::clone(&suppressions);

        let forward = forward_reconfigurable_until_shutdown(
            &capture,
            &*connection,
            MessageId(200),
            topology,
            shutdown,
            InputForwardingConfig {
                emergency_stop_keycode: 41,
                remote_focus_timeout_millis: 3_000,
            },
            move |suppressed| suppressions_for_callback.lock().unwrap().push(suppressed),
        );
        let drive = async move {
            wait_for_sent_events(&connection_for_driver, 2).await;
            shutdown_sender.send(true).unwrap();
        };

        let (result, ()) = tokio::join!(forward, drive);
        result.unwrap();
        let events: Vec<_> = connection
            .sent
            .lock()
            .unwrap()
            .clone()
            .into_iter()
            .map(|envelope| decode_input_event(envelope).unwrap())
            .collect();
        assert_eq!(
            events,
            vec![
                InputEvent::PointerMove { x: 0.0, y: 0.5 },
                InputEvent::KeyPress(0xE1),
                InputEvent::KeyRelease(0xE1),
            ]
        );
        assert_eq!(suppressions.lock().unwrap().as_slice(), &[true, false]);
        assert_eq!(connection.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn injector_shutdown_releases_locally_held_inputs_and_closes() {
        let (event_sender, event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let connection = ChannelConnection {
            sent: Mutex::new(Vec::new()),
            recv: tokio::sync::Mutex::new(event_receiver),
            closed: AtomicUsize::new(0),
        };
        event_sender
            .send(encode_input_event(MessageId(1), InputEvent::KeyPress(0xE1)).unwrap())
            .unwrap();
        event_sender
            .send(
                encode_input_event(
                    MessageId(2),
                    InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
                )
                .unwrap(),
            )
            .unwrap();
        let injector = RecordingInjector::default();
        let (shutdown_sender, shutdown) = tokio::sync::watch::channel(false);

        let inject = inject_until_shutdown(&connection, &injector, shutdown);
        let drive = async {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if injector.events.lock().unwrap().len() >= 2 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            shutdown_sender.send(true).unwrap();
        };

        let (result, ()) = tokio::join!(inject, drive);
        result.unwrap();
        assert_eq!(
            injector.events.lock().unwrap().as_slice(),
            &[
                InputEvent::KeyPress(0xE1),
                InputEvent::ButtonPress(nexkvm_input::MouseButton::Left),
                InputEvent::ButtonRelease(nexkvm_input::MouseButton::Left),
                InputEvent::KeyRelease(0xE1),
            ]
        );
        assert_eq!(connection.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn duplicate_forwarder_rejection_closes_the_physical_connection() {
        let gate = Arc::new(InputForwarderGate::default());
        let _winner = gate.try_acquire().unwrap();
        let connection = MemoryConnection::default();

        assert!(gate.try_acquire().is_none());
        close_input_connection(&connection).await;

        assert_eq!(connection.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn task_supervisor_signals_and_awaits_registered_cleanup() {
        let supervisor = InputTaskSupervisor::new();
        let mut shutdown = supervisor.subscribe();
        let cleaned = Arc::new(AtomicBool::new(false));
        let cleaned_by_task = Arc::clone(&cleaned);
        supervisor.spawn(async move {
            shutdown.wait_for(|requested| *requested).await.unwrap();
            cleaned_by_task.store(true, Ordering::SeqCst);
        });

        let completed = supervisor.shutdown(std::time::Duration::from_secs(1)).await;

        assert!(completed);
        assert!(cleaned.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn task_supervisor_never_detaches_tasks_registered_after_shutdown() {
        let supervisor = InputTaskSupervisor::new();
        assert!(supervisor.shutdown(std::time::Duration::from_secs(1)).await);
        let ran = Arc::new(AtomicBool::new(false));
        let ran_by_task = Arc::clone(&ran);

        supervisor.spawn(async move {
            ran_by_task.store(true, Ordering::SeqCst);
        });
        tokio::task::yield_now().await;

        assert!(!ran.load(Ordering::SeqCst));
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
