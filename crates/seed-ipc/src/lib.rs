//! SEED Sync IPC contract: the request/response/event types exchanged between the
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
    /// Set the global "pause all activity" switch: suspends sync for every share.
    PauseAll,
    /// Clear the global pause switch *and* every per-share pause flag, so a full
    /// resume gets everything syncing again.
    ResumeAll,
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
    /// This device's custom relay configuration (empty = iroh defaults).
    GetRelays,
    /// Replace the custom relay configuration. The daemon validates, persists,
    /// and applies it to the live endpoint (no restart needed).
    SetRelays {
        settings: RelaySettings,
    },
    /// Probe a relay server (URL + optional access token) by connecting to it
    /// from a throwaway endpoint. Does not touch the saved settings.
    TestRelay {
        url: String,
        token: Option<String>,
    },
    /// Open long-term-health episodes for a share's members (poll counterpart
    /// of [`IpcEvent::PeerHealth`], for the CLI / soak / GUI detail views).
    GetPeerHealth {
        share_id: ShareId,
    },
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
    PeerHealth(Vec<PeerHealthInfo>),
    DeviceName(String),
    Relays(RelaySettings),
    /// Result of [`IpcRequest::TestRelay`]: whether the probe endpoint came
    /// online through the relay, plus a human-readable detail line.
    RelayTest {
        ok: bool,
        detail: String,
    },
    NodeAddr(String),
    Ok,
    Err(String),
}

/// One open long-term-health episode (see [`IpcRequest::GetPeerHealth`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerHealthInfo {
    /// Full endpoint id of the member; empty when it is this device.
    pub node_id: String,
    pub name: Option<String>,
    pub online: bool,
    /// The member's last self-reported sync percent.
    pub percent: u8,
    /// Accrued online-degraded seconds so far this episode.
    pub unhealthy_secs: i64,
    /// Whether an alert has already fired for this episode.
    pub alerted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcEvent {
    Throughput {
        down_bps: u64,
        up_bps: u64,
    },
    ShareListChanged,
    LastUpdated {
        share_id: ShareId,
        /// Unix seconds.
        ts: i64,
    },
    /// A member of `share_id` crossed the long-term unhealthy threshold (or a
    /// renotify is due, or it recovered). Emitted by the daemon's detector on
    /// the node itself (`is_self`) and on every master observing the member;
    /// the GUI turns it into a toast + OS notification.
    PeerHealth {
        share_id: ShareId,
        /// Display name of the share (its folder name).
        share_name: String,
        /// Full endpoint id of the member; empty when it is this device.
        node_id: String,
        /// The member's self-chosen display name, if announced.
        name: Option<String>,
        /// The member's last self-reported sync percent.
        percent: u8,
        /// Accrued online-degraded seconds (0 for a recovery).
        unhealthy_secs: i64,
        is_self: bool,
        /// True = the previously-alerted member is back in sync.
        recovered: bool,
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
    /// This member's manifest has disagreed with an online peer's for longer than
    /// the settle window: members hold different filesets. Surfaced distinctly so a
    /// silent multi-member divergence can't hide behind "Healthy".
    OutOfSync,
    /// This node can reach **no member of the share at all** — it is partitioned from
    /// the pool (and so is neither syncing nor able to detect that it has diverged).
    ///
    /// Distinct from `Healthy` because it used to be indistinguishable from it: every
    /// peer comparison the engine makes is over the set of *online* peers, and all of
    /// them are vacuously true of an empty set, so a fully-partitioned node reported
    /// `Healthy 100%` — 100% of nothing. That is what hid the cold-join bootstrap
    /// failure (known-issues #16) on a live share for over a week.
    ///
    /// Not reported for a share this device created that nobody has joined yet: being
    /// alone there is the expected steady state, not a partition.
    NoPeers,
    /// This is a **master** share, but its write key could not be loaded from the OS
    /// keystore (a locked login keyring, a dismissed unlock prompt, an unavailable
    /// Secret Service). The share is held **inert** — not synced in either direction —
    /// until the key is available, and retried automatically.
    ///
    /// It is deliberately not "quietly read-only". Read-only is not a safe fallback
    /// for a master: a viewer treats the replica as authoritative and *reverts local
    /// edits*, so running a keyless master as a viewer silently destroys the user's own
    /// writes in their own share while every screen reports Healthy. Holding it inert
    /// is the only degradation that cannot lose data.
    KeyLocked,
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
    /// Files this node currently can't read or publish (locked/unreadable) and is
    /// retrying. Non-zero means the share is NOT fully settled even if `percent`
    /// looks high — surfaced so a stuck/locked file is never silently hidden behind
    /// "Healthy 100%".
    pub retrying: u32,
}

/// How this device currently reaches a member, snapshotted from the iroh
/// endpoint's active transport addresses when the peer list is served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerPath {
    /// An active peer-to-peer path (hole-punched or same-LAN) carries traffic.
    Direct,
    /// Traffic is relayed via the given relay host.
    Relay(String),
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
    /// This member's manifest fingerprint (0 = unknown). Members that agree on the
    /// fileset share the same value; a different value means this member disagrees.
    pub manifest_fp: u64,
    /// Seconds this member has been *online but degraded* in its current
    /// long-term-health episode (0 = no open episode). Accrues across offline
    /// gaps (pause-not-reset); drives the "unhealthy 12h+" notifications.
    #[serde(default)]
    pub unhealthy_secs: i64,
    /// How this device reaches the member right now; `None` when unknown
    /// (offline, this device itself, or no recent iroh connection).
    #[serde(default)]
    pub path: Option<PeerPath>,
    /// Unix secs of the last doc-sync **this device initiated** to the member
    /// that succeeded (0 = none this session). With `last_dial_err_at` this
    /// tells "they reach us but we cannot reach them" apart from "offline" —
    /// the signature the transport-repair ladder acts on (known-issues #36).
    #[serde(default)]
    pub last_dial_ok: i64,
    /// Error text of the most recent failed outbound dial to the member.
    #[serde(default)]
    pub last_dial_err: Option<String>,
    /// Unix secs of that failure (0 = none this session).
    #[serde(default)]
    pub last_dial_err_at: i64,
}

/// One user-configured custom relay server (a self-hosted iroh relay).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayServer {
    /// `https://host[:port]` of the relay.
    pub url: String,
    /// Optional access token, sent as `Authorization: Bearer <token>` when
    /// connecting. Stored locally on this device only.
    #[serde(default)]
    pub token: Option<String>,
}

/// How custom relays combine with the public iroh relays.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayPolicy {
    /// Use the custom relays; fall back to the public relays only while none
    /// of the custom ones is reachable.
    #[default]
    Preferred,
    /// Use the custom relays exclusively — never contact the public relays.
    Only,
}

/// This device's custom relay configuration. Empty `servers` = iroh defaults.
///
/// Per-device and deliberately NOT synced between share members: a relay with
/// a token rejects clients that don't have it, so every member that should use
/// the relay configures it locally.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaySettings {
    #[serde(default)]
    pub servers: Vec<RelayServer>,
    #[serde(default)]
    pub mode: RelayPolicy,
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
    fn roundtrip_relays() {
        let settings = RelaySettings {
            servers: vec![RelayServer {
                url: "https://relay.example.com:8443".into(),
                token: Some("s3cret".into()),
            }],
            mode: RelayPolicy::Only,
        };
        let frame = Frame {
            id: 11,
            body: Message::Request(IpcRequest::SetRelays {
                settings: settings.clone(),
            }),
        };
        let back = decode(&encode(&frame).unwrap()).unwrap();
        match back.body {
            Message::Request(IpcRequest::SetRelays { settings: s }) => assert_eq!(s, settings),
            _ => panic!("wrong variant"),
        }
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
