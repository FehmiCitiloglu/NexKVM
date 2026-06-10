//! Service announcement: the metadata a device broadcasts so peers can find it.
//!
//! The same logical announcement is carried over two backends:
//! - **UDP broadcast** — encoded as a magic-prefixed JSON datagram
//!   ([`ServiceAnnouncement::encode`] / [`ServiceAnnouncement::decode`]).
//! - **mDNS/DNS-SD** — encoded as TXT record key/value pairs
//!   ([`ServiceAnnouncement::to_txt`] / [`ServiceAnnouncement::from_txt`]).
//!
//! JSON is used for the UDP payload deliberately: announcements are small and
//! infrequent, so wire compactness is irrelevant, while forward/backward
//! compatibility (unknown fields tolerated) and debuggability matter. The
//! datagram carries a magic prefix so stray traffic on the discovery port is
//! cheaply rejected before any parsing.

use std::collections::HashMap;

use nexkvm_core::identity::{DeviceId, DeviceInfo, OsKind};
use serde::{Deserialize, Serialize};

use crate::DiscoveryError;

/// DNS-SD service type for nexkvm peers.
pub const SERVICE_TYPE: &str = "_nexkvm._udp.local.";

/// Default port peers listen on for discovery datagrams (distinct from the
/// session listen port so discovery and data planes never collide).
pub const DEFAULT_DISCOVERY_PORT: u16 = 47_655;

/// Magic prefix on every UDP announcement datagram. The trailing version digit
/// lets the wire format evolve without ambiguity.
const MAGIC: &[u8] = b"NEXKVM/disc/1\n";

/// Metadata a device advertises on the LAN.
///
/// This is *advertised*, not *authenticated*: anything here may be spoofed. The
/// `fingerprint` lets a receiver match the announcement against its trust store
/// to decide whether to auto-reconnect, but the binding is only proven by the
/// cryptographic handshake after connecting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceAnnouncement {
    /// Advertised device identity/metadata.
    pub info: DeviceInfo,
    /// Port to dial for an actual session (the data plane, not discovery).
    pub port: u16,
    /// Optional short fingerprint of the device's public key, for trust
    /// matching prior to connecting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Protocol major version, so incompatible peers can be filtered early.
    pub proto_major: u16,
}

impl ServiceAnnouncement {
    /// Build an announcement for `info` listening on `port`.
    #[must_use]
    pub fn new(info: DeviceInfo, port: u16, proto_major: u16) -> Self {
        Self {
            info,
            port,
            fingerprint: None,
            proto_major,
        }
    }

    /// Attach a public-key fingerprint for trust matching.
    #[must_use]
    pub fn with_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.fingerprint = Some(fingerprint.into());
        self
    }

    /// This device's id (shorthand).
    #[must_use]
    pub fn device_id(&self) -> DeviceId {
        self.info.id
    }

    /// Encode to a magic-prefixed JSON datagram for UDP broadcast.
    ///
    /// # Errors
    /// Returns [`DiscoveryError::Codec`] if JSON serialization fails (should not
    /// happen for well-formed input).
    pub fn encode(&self) -> Result<Vec<u8>, DiscoveryError> {
        let json = serde_json::to_vec(self).map_err(|e| DiscoveryError::Codec(e.to_string()))?;
        let mut out = Vec::with_capacity(MAGIC.len() + json.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&json);
        Ok(out)
    }

    /// Decode a UDP datagram, rejecting anything without the magic prefix.
    ///
    /// # Errors
    /// Returns [`DiscoveryError::Codec`] if the prefix is missing or the JSON
    /// body is malformed.
    pub fn decode(datagram: &[u8]) -> Result<Self, DiscoveryError> {
        let body = datagram
            .strip_prefix(MAGIC)
            .ok_or_else(|| DiscoveryError::Codec("missing announcement magic".into()))?;
        serde_json::from_slice(body).map_err(|e| DiscoveryError::Codec(e.to_string()))
    }

    /// Render to mDNS TXT key/value properties.
    #[must_use]
    pub fn to_txt(&self) -> HashMap<String, String> {
        let mut txt = HashMap::new();
        txt.insert("id".into(), self.info.id.to_string());
        txt.insert("name".into(), self.info.name.clone());
        txt.insert("os".into(), os_to_str(self.info.os).into());
        txt.insert("port".into(), self.port.to_string());
        txt.insert("ver".into(), self.proto_major.to_string());
        if let Some(fp) = &self.fingerprint {
            txt.insert("fp".into(), fp.clone());
        }
        txt
    }

    /// Parse from mDNS TXT properties.
    ///
    /// # Errors
    /// Returns [`DiscoveryError::Codec`] if required keys are missing or invalid.
    pub fn from_txt(txt: &HashMap<String, String>) -> Result<Self, DiscoveryError> {
        let get = |k: &str| {
            txt.get(k)
                .ok_or_else(|| DiscoveryError::Codec(format!("missing txt key `{k}`")))
        };
        let id = get("id")?
            .parse()
            .map_err(|_| DiscoveryError::Codec("invalid device id".into()))?;
        let port = get("port")?
            .parse()
            .map_err(|_| DiscoveryError::Codec("invalid port".into()))?;
        let proto_major = get("ver")?
            .parse()
            .map_err(|_| DiscoveryError::Codec("invalid version".into()))?;
        Ok(Self {
            info: DeviceInfo {
                id: DeviceId(id),
                name: get("name")?.clone(),
                os: os_from_str(get("os")?),
            },
            port,
            fingerprint: txt.get("fp").cloned(),
            proto_major,
        })
    }
}

fn os_to_str(os: OsKind) -> &'static str {
    match os {
        OsKind::Windows => "windows",
        OsKind::MacOs => "macos",
        OsKind::Linux => "linux",
        OsKind::Android => "android",
        OsKind::Ios => "ios",
        OsKind::Unknown => "unknown",
        _ => "unknown",
    }
}

fn os_from_str(s: &str) -> OsKind {
    match s {
        "windows" => OsKind::Windows,
        "macos" => OsKind::MacOs,
        "linux" => OsKind::Linux,
        "android" => OsKind::Android,
        "ios" => OsKind::Ios,
        _ => OsKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ServiceAnnouncement {
        let info = DeviceInfo::new("Alien's MacBook", OsKind::MacOs);
        ServiceAnnouncement::new(info, 47_654, 1).with_fingerprint("aa:bb:cc:dd")
    }

    #[test]
    fn udp_round_trips() {
        let ann = sample();
        let bytes = ann.encode().unwrap();
        let decoded = ServiceAnnouncement::decode(&bytes).unwrap();
        assert_eq!(decoded, ann);
    }

    #[test]
    fn rejects_datagram_without_magic() {
        let err = ServiceAnnouncement::decode(b"{\"not\":\"ours\"}").unwrap_err();
        assert!(matches!(err, DiscoveryError::Codec(_)));
    }

    #[test]
    fn txt_round_trips() {
        let ann = sample();
        let txt = ann.to_txt();
        let decoded = ServiceAnnouncement::from_txt(&txt).unwrap();
        assert_eq!(decoded, ann);
    }

    #[test]
    fn txt_missing_key_errors() {
        let mut txt = sample().to_txt();
        txt.remove("port");
        assert!(ServiceAnnouncement::from_txt(&txt).is_err());
    }
}
