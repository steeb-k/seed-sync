//! Per-share "presence": each member broadcasts its self-chosen display name and
//! its sync health to the other members of a share over an iroh-gossip topic, so
//! the GUI can show who's in the pool, how they're named, and how caught-up they
//! are.
//!
//! Why gossip and not iroh-docs entries: viewers hold a read-only doc capability
//! and cannot write doc entries, so presence can't ride the replica. Gossip is
//! ephemeral and any member may broadcast on the topic.
//!
//! Trust: a message is attributed to its gossip `delivered_from` endpoint id,
//! which iroh has already QUIC-authenticated. v1 does NOT sign the payload, so a
//! share member could spoof another member's *name/health* on this channel — it
//! cannot forge file content, which stays manifest-signed. Acceptable for now; a
//! later pass can sign `Presence` and pin endpoint-id ↔ name.

use std::sync::{Arc, Mutex as StdMutex};

use anyhow::Context;
use bytes::Bytes;
use futures_lite::StreamExt;
use iroh::EndpointId;
use iroh_gossip::api::{Event, GossipSender};
use iroh_gossip::net::Gossip;
use iroh_gossip::proto::TopicId;
use serde::{Deserialize, Serialize};

use crate::engine::PeerRoster;

/// Current presence wire-format version.
pub const PRESENCE_V: u8 = 1;

/// Presence wire message (CBOR), broadcast periodically on a share's topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    /// Format version, for forward-compat.
    pub v: u8,
    /// Self-chosen display name (this device's global name).
    pub name: String,
    pub role: seed_ipc::Role,
    /// Highest manifest seqno the sender has applied (viewer) / published (master).
    pub seqno: u64,
    /// Sync health 0..=100 (100 = fully caught up).
    pub percent: u8,
    /// Sender's unix-seconds clock — informational / staleness only.
    pub ts: i64,
}

/// Derive a presence gossip topic for a share, domain-separated so it can't
/// collide with the topic iroh-docs derives from the same namespace.
pub fn presence_topic(share_id: &[u8; 32]) -> TopicId {
    let mut h = blake3::Hasher::new();
    h.update(b"seed-sync/presence/v1");
    h.update(share_id);
    TopicId::from_bytes(*h.finalize().as_bytes())
}

/// Encode a presence message to gossip bytes.
pub fn encode(p: &Presence) -> Bytes {
    let mut buf = Vec::new();
    // Presence is tiny and infallible to serialize; the error case can't occur.
    let _ = ciborium::into_writer(p, &mut buf);
    Bytes::from(buf)
}

/// Decode a presence message from gossip bytes.
pub fn decode(b: &[u8]) -> anyhow::Result<Presence> {
    ciborium::from_reader(b).context("decode presence")
}

/// Live handle to a share's presence gossip: a cloneable sender (used to
/// broadcast off the engine lock) plus the receive task, aborted on drop.
pub(crate) struct PresenceHandle {
    pub(crate) sender: GossipSender,
    recv: tokio::task::AbortHandle,
}

impl Drop for PresenceHandle {
    fn drop(&mut self) {
        self.recv.abort();
    }
}

/// A pending presence broadcast: the engine builds these under its lock (cloning
/// the cheap `GossipSender` and pre-encoding the message), the daemon sends them
/// off-lock so gossip IO never blocks the engine mutex. Opaque to the daemon so
/// it needs no iroh-gossip dependency.
pub struct PresenceBroadcast {
    sender: GossipSender,
    bytes: Bytes,
}

impl PresenceBroadcast {
    pub(crate) fn new(sender: GossipSender, p: &Presence) -> Self {
        Self {
            sender,
            bytes: encode(p),
        }
    }

    /// Send the broadcast (best-effort; gossip delivery is unreliable by design).
    pub async fn send(self) {
        let _ = self.sender.broadcast(self.bytes).await;
    }
}

/// Subscribe to a share's presence topic and spawn a detached task that folds
/// incoming presence into `roster`. Non-blocking — like `doc.start_sync`, it does
/// not wait for any neighbor to be reachable.
pub(crate) async fn spawn_presence(
    gossip: &Gossip,
    topic: TopicId,
    bootstrap: Vec<EndpointId>,
    self_id: EndpointId,
    roster: Arc<StdMutex<PeerRoster>>,
) -> anyhow::Result<PresenceHandle> {
    let sub = gossip
        .subscribe(topic, bootstrap)
        .await
        .context("gossip subscribe")?;
    let (sender, mut receiver) = sub.split();
    let task = tokio::spawn(async move {
        while let Some(ev) = receiver.next().await {
            let Ok(ev) = ev else { continue };
            match ev {
                Event::Received(m) => {
                    // Ignore our own broadcasts echoed back by the swarm.
                    if m.delivered_from == self_id {
                        continue;
                    }
                    if let Ok(p) = decode(&m.content) {
                        if let Ok(mut r) = roster.lock() {
                            r.note_presence(&m.delivered_from.to_string(), p);
                        }
                    }
                }
                Event::NeighborUp(id) => {
                    if let Ok(mut r) = roster.lock() {
                        r.note(&id.to_string(), Some(true));
                    }
                }
                Event::NeighborDown(id) => {
                    if let Ok(mut r) = roster.lock() {
                        r.note(&id.to_string(), Some(false));
                    }
                }
                Event::Lagged => {}
            }
        }
    });
    Ok(PresenceHandle {
        sender,
        recv: task.abort_handle(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_is_deterministic_and_domain_separated() {
        let id = [7u8; 32];
        assert_eq!(presence_topic(&id), presence_topic(&id));
        // Must differ from a bare blake3(share_id) so it can't collide with a
        // topic derived straight from the namespace id.
        let bare = TopicId::from_bytes(*blake3::hash(&id).as_bytes());
        assert_ne!(presence_topic(&id), bare);
    }

    #[test]
    fn presence_roundtrips() {
        let p = Presence {
            v: PRESENCE_V,
            name: "Desktop".into(),
            role: seed_ipc::Role::Viewer,
            seqno: 42,
            percent: 73,
            ts: 1700,
        };
        let back = decode(&encode(&p)).unwrap();
        assert_eq!(back.name, "Desktop");
        assert_eq!(back.seqno, 42);
        assert_eq!(back.percent, 73);
        assert!(matches!(back.role, seed_ipc::Role::Viewer));
    }
}
