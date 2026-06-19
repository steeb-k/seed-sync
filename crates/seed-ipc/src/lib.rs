//! Seed Sync IPC contract: the request/response/event types exchanged between the
//! unprivileged GUI/tray clients and the `seed-daemon` over a local socket
//! (Unix domain socket on Linux, named pipe on Windows).
//!
//! This crate is deliberately dependency-light (no iroh, no gtk) so a protocol
//! tweak does not force a heavy rebuild of either side.

use serde::{Deserialize, Serialize};

#[cfg(feature = "transport")]
pub mod transport;

/// Machine-wide data directory on Windows: `%PROGRAMDATA%\SeedSync`.
///
/// The daemon may run as a LocalSystem service while the GUI/CLI run as the
/// logged-in user. Per-account profile dirs (what `directories` returns) would
/// resolve differently for each, so they'd compute different socket paths — and
/// since the pipe name is hashed from that path, they'd never meet. A fixed
/// machine-wide location makes both sides derive the *same* socket. The daemon
/// owns this directory exclusively (DB + iroh store); the GUI only needs the
/// socket path, never the directory's contents.
#[cfg(windows)]
pub fn machine_data_dir() -> std::path::PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\ProgramData"));
    base.join("SeedSync")
}

/// The machine-wide IPC socket path on Windows (`%PROGRAMDATA%\SeedSync\seed.sock`).
#[cfg(windows)]
pub fn machine_socket() -> std::path::PathBuf {
    machine_data_dir().join("seed.sock")
}

/// Opaque 32-byte share identifier (`BLAKE3(master_pubkey)`), hex-encoded for display.
pub type ShareId = String;

/// A single frame on the wire: a correlation id plus a body. `id == 0` marks a
/// server-pushed [`IpcEvent`]; nonzero ids correlate a [`IpcRequest`] with its
/// [`IpcResponse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub id: u64,
    pub body: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    Request(IpcRequest),
    Response(IpcResponse),
    Event(IpcEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    ListShares,
    CreateShare {
        folder: String,
        generate_ignore: bool,
        ignore: Vec<String>,
    },
    AddShare {
        key: String,
        folder: String,
        /// Optional dialable bootstrap address (an iroh endpoint-ticket string)
        /// for the joining peer, used when DNS discovery isn't available.
        bootstrap: Option<String>,
    },
    /// Trigger a master republish (rescan + sign) for a share.
    Publish {
        share_id: ShareId,
    },
    /// Return this daemon's dialable address as an endpoint-ticket string, so a
    /// peer can be handed it as `AddShare { bootstrap }`.
    NodeAddr,
    Pause {
        share_id: ShareId,
    },
    Resume {
        share_id: ShareId,
    },
    RemoveShare {
        share_id: ShareId,
        delete_files: bool,
    },
    /// Returns keys only when the local role for the share is `Master`.
    RevealKeys {
        share_id: ShareId,
    },
    GetPeers {
        share_id: ShareId,
    },
    /// This device's display name (default: the machine hostname), shown to the
    /// other members of every share. One global name per device.
    GetDeviceName,
    SetDeviceName {
        name: String,
    },
    /// Upgrade this connection to also receive server-pushed [`IpcEvent`]s.
    Subscribe,
    GetSettings,
    SetSettings(Settings),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    Shares(Vec<ShareSummary>),
    ShareCreated {
        share_id: ShareId,
        master_key: String,
        viewer_key: String,
    },
    ShareAdded {
        share_id: ShareId,
    },
    Keys {
        master_key: Option<String>,
        viewer_key: String,
    },
    Peers(Vec<PeerInfo>),
    DeviceName(String),
    Settings(Settings),
    NodeAddr(String),
    Ok,
    Err(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcEvent {
    ShareStatus {
        share_id: ShareId,
        status: ShareStatus,
        percent: u8,
    },
    Throughput {
        down_bps: u64,
        up_bps: u64,
    },
    Membership {
        share_id: ShareId,
        online: u32,
        total: u32,
    },
    ShareListChanged,
    LastUpdated {
        share_id: ShareId,
        /// Unix seconds.
        ts: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Master,
    Viewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareStatus {
    Healthy,
    Syncing,
    /// Master is hashing + importing the local folder into the content store
    /// (the initial publish or a republish). `percent`/`indexed_bytes` track it.
    Indexing,
    Paused,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareSummary {
    pub share_id: ShareId,
    pub name: String,
    pub folder: String,
    pub role: Role,
    pub status: ShareStatus,
    pub percent: u8,
    pub online: u32,
    pub total: u32,
    pub paused: bool,
    /// While `status == Indexing`: bytes imported so far and the total to import.
    /// Both 0 otherwise.
    pub indexed_bytes: u64,
    pub index_total: u64,
    /// Unix seconds of the last successful sync/publish for this share (0 if never).
    pub last_updated: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Short form of the iroh node id.
    pub node_id: String,
    /// Self-chosen display name from the member's presence; `None` => the GUI
    /// falls back to `node_id`.
    pub name: Option<String>,
    pub role: Role,
    pub online: bool,
    /// Unix seconds of last presence heartbeat.
    pub last_seen: i64,
    /// Highest manifest seqno this peer reports having.
    pub have_seqno: u64,
    /// Sync health 0..=100 (100 = fully caught up; lower while downloading or
    /// behind on an older version).
    pub percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Use iroh default relays + optional self-hosted relay URL for NAT fallback.
    pub use_relays: bool,
    pub custom_relay_url: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            use_relays: true,
            custom_relay_url: None,
        }
    }
}

/// Errors at the framing/codec layer.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("cbor encode: {0}")]
    Encode(String),
    #[error("cbor decode: {0}")]
    Decode(String),
}

/// Encode a frame to CBOR bytes (without length prefix; the transport layer adds framing).
pub fn encode(frame: &Frame) -> Result<Vec<u8>, CodecError> {
    let mut buf = Vec::new();
    ciborium::into_writer(frame, &mut buf).map_err(|e| CodecError::Encode(e.to_string()))?;
    Ok(buf)
}

/// Decode a frame from CBOR bytes.
pub fn decode(bytes: &[u8]) -> Result<Frame, CodecError> {
    ciborium::from_reader(bytes).map_err(|e| CodecError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_request() {
        let frame = Frame {
            id: 7,
            body: Message::Request(IpcRequest::AddShare {
                key: "seedv1abc".into(),
                folder: "/tmp/share".into(),
                bootstrap: None,
            }),
        };
        let bytes = encode(&frame).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back.id, 7);
        matches!(back.body, Message::Request(IpcRequest::AddShare { .. }));
    }

    #[test]
    fn roundtrip_device_name() {
        let frame = Frame {
            id: 9,
            body: Message::Request(IpcRequest::SetDeviceName {
                name: "Desktop".into(),
            }),
        };
        let back = decode(&encode(&frame).unwrap()).unwrap();
        match back.body {
            Message::Request(IpcRequest::SetDeviceName { name }) => assert_eq!(name, "Desktop"),
            _ => panic!("wrong variant"),
        }
    }
}
