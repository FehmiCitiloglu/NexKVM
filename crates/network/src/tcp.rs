//! Working TCP transport (the universal fallback).
//!
//! Implements [`Transport`]/[`Connection`] over `tokio::net::TCP`, framing
//! [`Envelope`]s with [`nexkvm_protocol::FrameCodec`] + [`crate::wire`]. Nagle is
//! disabled (`TCP_NODELAY`) so small input messages are not delayed by the
//! kernel — latency batching is owned explicitly by [`crate::buffer`] instead.
//!
//! # Security note
//! This layer moves bytes; it does **not** itself add TLS. Confidentiality and
//! integrity are provided by the [`nexkvm_crypto`](nexkvm_crypto) session layer,
//! which seals the [`Envelope`] body before it reaches the transport. A TLS
//! wrapper (rustls) for the TCP path is a follow-up in the security phase; the
//! QUIC path already carries TLS 1.3 natively.
//!
//! # Concurrency
//! `send`/`recv` take `&self`. The stream is split into independent read/write
//! halves, each behind its own `tokio::sync::Mutex`. The two directions never
//! contend, and only one in-flight operation per direction is serialized — no
//! single "giant" lock spans unrelated work.

use std::net::SocketAddr;

use async_trait::async_trait;
use bytes::BytesMut;
use nexkvm_protocol::{Envelope, FrameCodec};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::error::NetworkError;
use crate::transport::{Connection, Transport, TransportKind};
use crate::wire;

/// Read buffer growth chunk.
const READ_CHUNK: usize = 8 * 1024;

/// A TCP-backed connection to a peer.
#[derive(Debug)]
pub struct TcpConnection {
    peer: SocketAddr,
    writer: Mutex<OwnedWriteHalf>,
    reader: Mutex<ReadState>,
}

#[derive(Debug)]
struct ReadState {
    half: OwnedReadHalf,
    buf: BytesMut,
}

impl TcpConnection {
    fn new(stream: TcpStream) -> Result<Self, NetworkError> {
        // Disable Nagle: we control batching ourselves; Nagle would add latency
        // to small input packets.
        stream.set_nodelay(true)?;
        let peer = stream.peer_addr()?;
        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            peer,
            writer: Mutex::new(write_half),
            reader: Mutex::new(ReadState {
                half: read_half,
                buf: BytesMut::with_capacity(READ_CHUNK),
            }),
        })
    }
}

#[async_trait]
impl Connection for TcpConnection {
    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
        // Encode the envelope, then length-prefix it for the stream.
        let mut payload = BytesMut::new();
        wire::encode_envelope(&envelope, &mut payload);
        let mut framed = BytesMut::new();
        FrameCodec.encode(&payload, &mut framed)?;

        let mut writer = self.writer.lock().await;
        writer.write_all(&framed).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        let mut state = self.reader.lock().await;
        // Split the borrow so the buffer and the socket half can be borrowed
        // mutably at the same time within the loop.
        let ReadState { half, buf } = &mut *state;
        loop {
            // Try to decode a complete frame already buffered.
            if let Some(frame) = FrameCodec.decode(buf)? {
                return wire::decode_envelope(frame).map_err(NetworkError::from);
            }
            // Otherwise read more bytes.
            let n = half.read_buf(buf).await?;
            if n == 0 {
                return Err(NetworkError::Closed);
            }
        }
    }

    async fn close(&self) -> Result<(), NetworkError> {
        let mut writer = self.writer.lock().await;
        writer.shutdown().await?;
        Ok(())
    }
}

/// A TCP transport: binds a listener and dials outbound connections.
#[derive(Debug)]
pub struct TcpTransport {
    listener: TcpListener,
}

impl TcpTransport {
    /// Bind a listener on `addr`.
    ///
    /// # Errors
    /// Returns [`NetworkError::Io`] if binding fails.
    pub async fn bind(addr: SocketAddr) -> Result<Self, NetworkError> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener })
    }

    /// The local address the listener is bound to.
    ///
    /// # Errors
    /// Returns [`NetworkError::Io`] if the address cannot be resolved.
    pub fn local_addr(&self) -> Result<SocketAddr, NetworkError> {
        Ok(self.listener.local_addr()?)
    }
}

#[async_trait]
impl Transport for TcpTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    async fn connect(&self, addr: SocketAddr) -> Result<Box<dyn Connection>, NetworkError> {
        let stream = TcpStream::connect(addr).await?;
        Ok(Box::new(TcpConnection::new(stream)?))
    }

    async fn accept(&self) -> Result<Box<dyn Connection>, NetworkError> {
        let (stream, _addr) = self.listener.accept().await?;
        Ok(Box::new(TcpConnection::new(stream)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use nexkvm_protocol::{MessageId, MessageKind, PROTOCOL_VERSION};

    fn loopback() -> SocketAddr {
        // Port 0 → OS picks a free port.
        "127.0.0.1:0".parse().unwrap()
    }

    #[tokio::test]
    async fn round_trips_envelopes_over_loopback() {
        let server = TcpTransport::bind(loopback()).await.unwrap();
        let addr = server.local_addr().unwrap();

        let client = TcpTransport::bind(loopback()).await.unwrap();

        // Accept on the server side concurrently with the client dialing.
        let server_task = tokio::spawn(async move { server.accept().await.unwrap() });
        let client_conn = client.connect(addr).await.unwrap();
        let server_conn = server_task.await.unwrap();

        let msg = Envelope::new(
            PROTOCOL_VERSION,
            MessageId(7),
            MessageKind::Clipboard,
            Bytes::from_static(b"hello over tcp"),
        );
        client_conn.send(msg.clone()).await.unwrap();

        let got = server_conn.recv().await.unwrap();
        assert_eq!(got.id, msg.id);
        assert_eq!(got.kind, msg.kind);
        assert_eq!(got.body, msg.body);
    }

    #[tokio::test]
    async fn recv_reports_closed_on_peer_shutdown() {
        let server = TcpTransport::bind(loopback()).await.unwrap();
        let addr = server.local_addr().unwrap();
        let client = TcpTransport::bind(loopback()).await.unwrap();

        let server_task = tokio::spawn(async move { server.accept().await.unwrap() });
        let client_conn = client.connect(addr).await.unwrap();
        let server_conn = server_task.await.unwrap();

        client_conn.close().await.unwrap();
        drop(client_conn);

        let err = server_conn.recv().await.unwrap_err();
        assert!(matches!(err, NetworkError::Closed));
    }
}
