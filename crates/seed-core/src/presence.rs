//! Per-share "presence": each member broadcasts its self-chosen display name and
//! its sync health to the other members of a share over an iroh-gossip topic, so
//! the GUI can show who's in the pool, how they're named, and how caught-up they
//! are.
//!
//! Why gossip and not iroh-docs entries: viewers hold a read-only doc capability
//! and cannot write doc entries, so presence can't ride the replica. Gossip is
//! ephemeral and any member may broadcast on the topic.
//!
//! Delivery: a single broadcast is best-effort, but the swarm's one-shot bootstrap
//! is a fragile star (the creator subscribes with an empty bootstrap; leaves dial
//! only the creator), so in a 3+ member pool epidemic relay reaches members
//! asymmetrically. The engine therefore periodically reconnects the mesh toward
//! all-to-all via [`PresenceRejoin`] using the peers doc-sync has discovered, so
//! every member ends up a direct neighbor and `broadcast` reaches everyone.
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

/// Current presence wire-format version. v2 added [`Presence::from`] so reception
/// can attribute a message to its *originator* rather than the gossip relay hop.
pub const PRESENCE_V: u8 = 2;

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
    /// Originator's endpoint id (raw bytes). The gossip swarm relays messages
    /// through intermediate members, rewriting `Message::delivered_from` to the
    /// forwarding hop, so the transport can't tell us who actually *sent* this.
    /// Carrying the id in the payload makes attribution correct for any number of
    /// relay hops / members. `None` only for legacy v1 senders (serde default).
    #[serde(default)]
    pub from: Option<[u8; 32]>,
}

/// Resolve which member a presence message is *from*. Gossip relays rewrite
/// `Message::delivered_from` to the forwarding hop, so trust the originator id
/// carried in the payload (v2+) and fall back to the transport hop only for legacy
/// v1 senders that omit it. Independent of hop count, so it scales to any number of
/// masters/members.
pub(crate) fn presence_origin(p: &Presence, delivered_from: EndpointId) -> EndpointId {
    p.from
        .and_then(|b| EndpointId::from_bytes(&b).ok())
        .unwrap_or(delivered_from)
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

/// A pending request to actively connect this share's gossip subscription to a set
/// of known member endpoint ids. The engine builds these under its lock (cloning
/// the `GossipSender` + snapshotting the roster) and the daemon runs them off-lock.
///
/// Why this exists: `gossip.subscribe` bootstrap is one-shot, and the share creator
/// subscribes with an *empty* bootstrap (its own id is filtered out), while leaves
/// bootstrap only from the creator. That leaves a fragile star whose epidemic relay
/// delivers presence asymmetrically once there are 3+ members. Periodically calling
/// [`GossipSender::join_peers`] with every peer doc-sync has discovered grows the
/// mesh toward all-to-all, so `broadcast` reaches everyone directly.
pub struct PresenceRejoin {
    sender: GossipSender,
    peers: Vec<EndpointId>,
}

impl PresenceRejoin {
    pub(crate) fn new(sender: GossipSender, peers: Vec<EndpointId>) -> Self {
        Self { sender, peers }
    }

    /// Ask the gossip swarm to connect to these peers (best-effort; idempotent for
    /// peers already in the active view).
    pub async fn join(self) {
        let _ = self.sender.join_peers(self.peers).await;
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
                    if let Ok(p) = decode(&m.content) {
                        // Attribute by the payload's originator, not the relay hop
                        // (`delivered_from`): once messages cross >1 hop the hop is
                        // a forwarder, not the sender. This also lets us drop our
                        // own presence relayed back with a foreign `delivered_from`,
                        // which a bare `delivered_from == self_id` guard misses.
                        let origin = presence_origin(&p, m.delivered_from);
                        if origin == self_id {
                            continue;
                        }
                        if let Ok(mut r) = roster.lock() {
                            r.note_presence(&origin.to_string(), p);
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
            from: Some([9u8; 32]),
        };
        let back = decode(&encode(&p)).unwrap();
        assert_eq!(back.name, "Desktop");
        assert_eq!(back.seqno, 42);
        assert_eq!(back.percent, 73);
        assert!(matches!(back.role, seed_ipc::Role::Viewer));
        assert_eq!(back.from, Some([9u8; 32]));
    }

    /// A v1 message (no `from`) must still decode, defaulting `from` to `None` so
    /// reception falls back to the transport hop.
    #[test]
    fn presence_v1_decodes_without_from() {
        // Serialize a struct lacking `from` the way a v1 sender would have.
        #[derive(serde::Serialize)]
        struct PresenceV1 {
            v: u8,
            name: String,
            role: seed_ipc::Role,
            seqno: u64,
            percent: u8,
            ts: i64,
        }
        let v1 = PresenceV1 {
            v: 1,
            name: "Old".into(),
            role: seed_ipc::Role::Master,
            seqno: 1,
            percent: 100,
            ts: 1,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&v1, &mut buf).unwrap();
        let back = decode(&buf).unwrap();
        assert_eq!(back.name, "Old");
        assert_eq!(back.from, None);
    }

    /// Attribution must follow the originator id in the payload, not the gossip
    /// relay hop. This is what keeps a member from being shown under a relaying
    /// peer's id once the swarm grows past a direct link (any number of members).
    #[test]
    fn origin_prefers_payload_over_relay_hop() {
        // EndpointIds are ed25519 public keys, so derive valid ones from signing
        // keys rather than arbitrary bytes (which aren't on-curve).
        let key_id = |seed: [u8; 32]| {
            let bytes = ed25519_dalek::SigningKey::from_bytes(&seed)
                .verifying_key()
                .to_bytes();
            EndpointId::from_bytes(&bytes).unwrap()
        };
        let origin = key_id([1u8; 32]);
        let relay = key_id([2u8; 32]);
        let p = Presence {
            v: PRESENCE_V,
            name: "A".into(),
            role: seed_ipc::Role::Master,
            seqno: 0,
            percent: 100,
            ts: 0,
            from: Some(*origin.as_bytes()),
        };
        // delivered_from is the forwarding hop; attribution must ignore it.
        assert_eq!(presence_origin(&p, relay), origin);

        // Legacy v1 (no `from`) falls back to the transport hop.
        let v1 = Presence { from: None, ..p };
        assert_eq!(presence_origin(&v1, relay), relay);
    }
}
