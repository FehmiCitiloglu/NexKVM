//! QUIC transport (the preferred backend) via [`quinn`].
//!
//! QUIC is priority 0 in [`TransportKind`]: TLS 1.3 is built in, streams are
//! multiplexed (no head-of-line blocking across input/clipboard/file lanes), and
//! connection migration survives a device roaming networks.
//!
//! # This phase
//! A working bidirectional-stream connection: each [`Envelope`] is length-framed
//! over a single persistent bidirectional QUIC stream, mirroring the TCP path so
//! callers are identical. Per-`MessageKind` stream separation and unreliable
//! datagrams for the real-time input path are a follow-up optimization (the
//! research notes track this); they slot in behind the same [`Connection`] API.
//!
//! # Security
//! For the LAN/pairing model coklu uses **self-signed, device-keyed**
//! certificates pinned via the `crypto` trust store (TOFU-then-pinned), not a
//! public CA. This module currently wires a development certificate path; the
//! security phase replaces verification with trust-store pinning. The
//! `Envelope` body is additionally sealed by the `crypto` session layer.

use std::net::SocketAddr;

use async_trait::async_trait;
use bytes::BytesMut;
use coklu_protocol::{Envelope, FrameCodec};
use quinn::{Endpoint, RecvStream, SendStream};
use tokio::sync::Mutex;

use crate::error::NetworkError;
use crate::transport::{Connection, Transport, TransportKind};
use crate::wire;

const READ_CHUNK: usize = 8 * 1024;
/// ALPN protocol identifier negotiated on the QUIC/TLS handshake.
const ALPN: &[u8] = b"coklu/1";
/// Single byte the dialer writes to materialize the control stream so the
/// acceptor's `accept_bi` resolves. Carries the protocol major version.
const STREAM_PROLOGUE: u8 = 1;

impl From<quinn::ConnectionError> for NetworkError {
    fn from(e: quinn::ConnectionError) -> Self {
        match e {
            quinn::ConnectionError::ApplicationClosed(_)
            | quinn::ConnectionError::LocallyClosed
            | quinn::ConnectionError::ConnectionClosed(_) => NetworkError::Closed,
            quinn::ConnectionError::TimedOut => NetworkError::Timeout,
            other => NetworkError::Io(std::io::Error::other(other.to_string())),
        }
    }
}

/// A QUIC-backed connection to a peer.
#[derive(Debug)]
pub struct QuicConnection {
    peer: SocketAddr,
    conn: quinn::Connection,
    writer: Mutex<SendStream>,
    reader: Mutex<ReadState>,
}

#[derive(Debug)]
struct ReadState {
    stream: RecvStream,
    buf: BytesMut,
}

impl QuicConnection {
    fn new(conn: quinn::Connection, send: SendStream, recv: RecvStream) -> Self {
        Self {
            peer: conn.remote_address(),
            conn,
            writer: Mutex::new(send),
            reader: Mutex::new(ReadState {
                stream: recv,
                buf: BytesMut::with_capacity(READ_CHUNK),
            }),
        }
    }
}

#[async_trait]
impl Connection for QuicConnection {
    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    async fn send(&self, envelope: Envelope) -> Result<(), NetworkError> {
        let mut payload = BytesMut::new();
        wire::encode_envelope(&envelope, &mut payload);
        let mut framed = BytesMut::new();
        FrameCodec.encode(&payload, &mut framed)?;

        let mut writer = self.writer.lock().await;
        writer
            .write_all(&framed)
            .await
            .map_err(|e| NetworkError::Io(std::io::Error::other(e.to_string())))?;
        Ok(())
    }

    async fn recv(&self) -> Result<Envelope, NetworkError> {
        let mut state = self.reader.lock().await;
        let ReadState { stream, buf } = &mut *state;
        loop {
            if let Some(frame) = FrameCodec.decode(buf)? {
                return wire::decode_envelope(frame).map_err(NetworkError::from);
            }
            // Pull the next chunk from the QUIC stream.
            match stream
                .read_chunk(READ_CHUNK, true)
                .await
                .map_err(|e| NetworkError::Io(std::io::Error::other(e.to_string())))?
            {
                Some(chunk) => buf.extend_from_slice(&chunk.bytes),
                None => return Err(NetworkError::Closed),
            }
        }
    }

    async fn close(&self) -> Result<(), NetworkError> {
        // 0 = application-level "normal close" code.
        self.conn.close(0u32.into(), b"bye");
        Ok(())
    }
}

/// A QUIC transport: an endpoint that both dials and accepts.
#[derive(Debug)]
pub struct QuicTransport {
    endpoint: Endpoint,
}

impl QuicTransport {
    /// Bind a QUIC endpoint on `addr` configured for both client and server use.
    ///
    /// # Errors
    /// Returns [`NetworkError`] if the endpoint cannot be created or TLS config
    /// fails.
    pub fn bind(addr: SocketAddr) -> Result<Self, NetworkError> {
        ensure_crypto_provider();
        let (server_cfg, client_cfg) = tls::dev_configs()?;
        let mut endpoint = Endpoint::server(server_cfg, addr)?;
        endpoint.set_default_client_config(client_cfg);
        Ok(Self { endpoint })
    }

    /// The local socket address the endpoint is bound to.
    ///
    /// # Errors
    /// Returns [`NetworkError::Io`] if the address cannot be resolved.
    pub fn local_addr(&self) -> Result<SocketAddr, NetworkError> {
        Ok(self.endpoint.local_addr()?)
    }
}

#[async_trait]
impl Transport for QuicTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Quic
    }

    async fn connect(&self, addr: SocketAddr) -> Result<Box<dyn Connection>, NetworkError> {
        let connecting = self
            .endpoint
            .connect(addr, "coklu")
            .map_err(|e| NetworkError::Io(std::io::Error::other(e.to_string())))?;
        let conn = connecting.await?;
        // Dialer opens the first bidirectional stream. A zero-length write emits
        // no STREAM frame, so the acceptor's `accept_bi` would stall forever;
        // send a single prologue byte to materialize the stream on the wire.
        let (mut send, recv) = conn.open_bi().await.map_err(NetworkError::from)?;
        send.write_all(&[STREAM_PROLOGUE])
            .await
            .map_err(|e| NetworkError::Io(std::io::Error::other(e.to_string())))?;
        Ok(Box::new(QuicConnection::new(conn, send, recv)))
    }

    async fn accept(&self) -> Result<Box<dyn Connection>, NetworkError> {
        let incoming = self.endpoint.accept().await.ok_or(NetworkError::Closed)?;
        let conn = incoming.await?;
        let (send, mut recv) = conn.accept_bi().await.map_err(NetworkError::from)?;
        // Consume the dialer's prologue byte before handing back the connection.
        let mut prologue = [0u8; 1];
        recv.read_exact(&mut prologue)
            .await
            .map_err(|e| NetworkError::Io(std::io::Error::other(e.to_string())))?;
        Ok(Box::new(QuicConnection::new(conn, send, recv)))
    }
}

/// Development TLS configuration: a self-signed certificate with peer
/// verification disabled. **Replaced by trust-store pinning in the security
/// phase** — see module docs.
mod tls {
    use std::sync::Arc;

    use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
    use quinn::{ClientConfig, ServerConfig};
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    use crate::error::NetworkError;

    use super::ALPN;

    fn io(msg: impl ToString) -> NetworkError {
        NetworkError::Io(std::io::Error::other(msg.to_string()))
    }

    /// Build (server, client) configs sharing one self-signed cert.
    pub fn dev_configs() -> Result<(ServerConfig, ClientConfig), NetworkError> {
        let cert = rcgen::generate_simple_self_signed(vec!["coklu".into()]).map_err(io)?;
        let cert_der = CertificateDer::from(cert.cert.der().to_vec());
        let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der()).map_err(io)?;

        // Server side.
        let mut server_crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key_der)
            .map_err(io)?;
        server_crypto.alpn_protocols = vec![ALPN.to_vec()];
        let server_cfg = ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(server_crypto).map_err(io)?,
        ));

        // Client side: dev verifier accepts any cert (pinning lands later).
        let mut client_crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny))
            .with_no_client_auth();
        client_crypto.alpn_protocols = vec![ALPN.to_vec()];
        let client_cfg = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(client_crypto).map_err(io)?,
        ));

        Ok((server_cfg, client_cfg))
    }

    /// Dev-only verifier. **Do not ship**: the security phase swaps this for
    /// trust-store certificate pinning.
    #[derive(Debug)]
    struct AcceptAny;

    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::ED25519,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::RSA_PSS_SHA256,
            ]
        }
    }
}

// Ensure the default crypto provider is installed exactly once before any TLS
// config is built (rustls 0.23 requires an explicit process-wide provider).
fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use coklu_protocol::{MessageId, MessageKind, PROTOCOL_VERSION};

    fn loopback() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    #[tokio::test]
    async fn round_trips_envelopes_over_quic() {
        let server = QuicTransport::bind(loopback()).unwrap();
        let addr = server.local_addr().unwrap();
        let client = QuicTransport::bind(loopback()).unwrap();

        let server_task = tokio::spawn(async move { server.accept().await.unwrap() });
        let client_conn = client.connect(addr).await.unwrap();
        let server_conn = server_task.await.unwrap();

        let msg = Envelope::new(
            PROTOCOL_VERSION,
            MessageId(11),
            MessageKind::Input,
            Bytes::from_static(b"hello over quic"),
        );
        client_conn.send(msg.clone()).await.unwrap();

        let got = server_conn.recv().await.unwrap();
        assert_eq!(got.id, msg.id);
        assert_eq!(got.kind, msg.kind);
        assert_eq!(got.body, msg.body);
    }
}
