//! Share-key **rendezvous**: how a node holding nothing but a share key finds a
//! live master.
//!
//! # The problem this exists to solve
//!
//! A share key carries the *creating* device's endpoint id (`ShareKey::endpoint_id`),
//! stamped in at mint time. Before this module, that single id was the entire
//! bootstrap set for any node added from a bare key: if the creator was offline, a
//! joiner had nowhere to dial, and every mechanism that knows about the *other*
//! members (presence gossip, the `peer_names` cache, the `\x00m/` member registry)
//! is downstream of first contact and so never engaged. The design intent — any
//! master key holder can bootstrap a joiner — was not implemented; only the creator
//! could. That is known-issues #16, and it made the creating device a silent
//! single point of failure for the life of the share.
//!
//! # The mechanism
//!
//! Every **master** periodically publishes its own dialable [`EndpointAddr`] as a
//! [pkarr] signed packet whose name is the **share's public key** — signing it with
//! the **share seed** (`master_seed`), not with the device key. Any key holder
//! resolves that name to reach whichever master published most recently.
//!
//! Two properties fall out of using the share keypair as the rendezvous name, and
//! they are the whole reason it is the right key to use:
//!
//! - **Viewers can resolve but cannot publish.** Resolving needs only `master_pub`,
//!   which every key holder has; signing needs `master_seed`, which only masters
//!   have. `PkarrRelayClient::resolve` verifies the signature against the name it
//!   asked for, so a viewer can neither forge a record nor clobber the real one to
//!   deny bootstrap.
//! - **No designated device.** Every master publishes under the same name, so the
//!   record is last-writer-wins — which is not a flaw here but the self-healing
//!   property: a master that is *down* stops republishing, so the newest packet is
//!   by construction from a master that was alive one republish interval ago. There
//!   is no anchor to elect, keep up, or fail over.
//!
//! The packet describes an endpoint, but the endpoint it describes (the share key)
//! is not a real iroh node — dialing it would fail, since the master's TLS identity
//! is its own device key. So the publishing master's **endpoint id travels in
//! [`UserData`]** while the relay URL and direct addresses ride the record's normal
//! address fields; [`resolve`] recombines them into a full, dialable [`EndpointAddr`].
//! That makes the resolved address good enough to dial immediately, with no second
//! discovery round trip.
//!
//! # Privacy
//!
//! The record maps a share's public key to a master's addresses, and the pkarr
//! server operator sees that mapping. Anyone who can *resolve* it already holds a
//! share key (they are at least a viewer, and would learn peer addresses by
//! connecting anyway), so the exposure is to the server, not to the world. We use
//! n0's public pkarr server, which n0 supports for production use. Self-hosting is
//! a one-constant change here plus an `iroh-dns-server` next to the relay — worth
//! doing if a deployment must not reveal share-key → address mappings to a third
//! party, but deliberately not a setting today.
//!
//! [pkarr]: https://pkarr.org

use std::{str::FromStr, time::Duration};

use anyhow::{anyhow, Context, Result};
use iroh::{
    address_lookup::{
        pkarr::{PkarrRelayClient, N0_DNS_PKARR_RELAY_PROD},
        EndpointData, EndpointInfo, UserData,
    },
    Endpoint, EndpointAddr, EndpointId, SecretKey,
};
use iroh_docs::api::Doc;

/// TTL on the published record. Short: the record's whole job is to name a master
/// that is *currently* up, so a stale cached answer is worse than a fresh miss.
const TTL_SECS: u32 = 60;

/// How often each master republishes. This is the staleness bound on "the last
/// publisher was alive": a joiner that resolves the record reaches a master that
/// was up at most this long ago.
pub const REPUBLISH_SECS: i64 = 120;

/// How often an isolated node re-resolves the rendezvous. Faster than
/// [`REPUBLISH_SECS`] so a joiner picks up a newly-published master promptly,
/// but slow enough that a share with genuinely no live master isn't hammering
/// the pkarr server.
pub const LOOKUP_SECS: i64 = 60;

/// Network timeout for a single publish or resolve.
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// A pkarr client that borrows the endpoint's TLS trust anchors and DNS resolver,
/// so rendezvous traffic uses exactly the same trust configuration as the rest of
/// the stack rather than a second, independently-configured one.
fn client(endpoint: &Endpoint) -> Result<PkarrRelayClient> {
    let url = N0_DNS_PKARR_RELAY_PROD
        .parse()
        .expect("n0 pkarr relay url is a compile-time constant");
    let dns = endpoint
        .dns_resolver()
        .map_err(|e| anyhow!("endpoint closed: {e}"))?
        .clone();
    Ok(PkarrRelayClient::new(
        url,
        endpoint.tls_config().clone(),
        dns,
    ))
}

/// Publish this device's address under the share's public key. Masters only —
/// `seed` is the share's `master_seed`, and a viewer does not have it.
///
/// Public so the round-trip against the real pkarr server can be tested directly;
/// the engine drives it through [`RendezvousPublish`].
pub async fn publish(endpoint: &Endpoint, seed: [u8; 32]) -> Result<()> {
    let share_key = SecretKey::from_bytes(&seed);
    let addr = endpoint.addr();
    if addr.addrs.is_empty() {
        // Not homed on a relay and no direct addresses yet (startup, or offline).
        // Publishing an address-less record would overwrite a *usable* one from
        // another master with one nobody can dial — the one way last-writer-wins
        // could actually hurt.
        return Err(anyhow!("endpoint has no dialable address yet"));
    }

    // The record is *named* by the share key but *describes* this master, so the
    // master's own endpoint id has to travel in the record's payload — the name
    // can't carry it. UserData holds 245 bytes; an endpoint id is 52 chars.
    let user_data = UserData::from_str(&addr.id.to_string())
        .context("endpoint id does not fit in pkarr user data")?;
    let data = EndpointData::new(addr.addrs.into_iter().collect()).with_user_data(user_data);

    let info = EndpointInfo::from_parts(share_key.public(), data);
    let packet = info
        .to_pkarr_signed_packet(&share_key, TTL_SECS)
        .map_err(|e| anyhow!("sign rendezvous packet: {e}"))?;
    client(endpoint)?
        .publish(&packet)
        .await
        .map_err(|e| anyhow!("publish rendezvous packet: {e}"))?;
    Ok(())
}

/// Resolve whichever master published most recently. Every key holder can do this:
/// it needs only `master_pub`.
///
/// Public so the round-trip against the real pkarr server can be tested directly;
/// the engine drives it through [`RendezvousDial`].
pub async fn resolve(endpoint: &Endpoint, master_pub: [u8; 32]) -> Result<EndpointAddr> {
    let name = EndpointId::from_bytes(&master_pub).context("share pubkey is not a valid key")?;

    // `resolve` verifies the packet's signature against `name` — the share key — so
    // a record that comes back at all was signed by a holder of the master seed.
    let packet = client(endpoint)?
        .resolve(name)
        .await
        .map_err(|e| anyhow!("resolve rendezvous record: {e}"))?;
    let info = EndpointInfo::from_pkarr_signed_packet(&packet)
        .map_err(|e| anyhow!("parse rendezvous record: {e}"))?;

    let id: EndpointId = info
        .data
        .user_data()
        .ok_or_else(|| anyhow!("rendezvous record carries no endpoint id"))?
        .as_ref()
        .parse()
        .context("rendezvous record's endpoint id is malformed")?;

    Ok(EndpointAddr {
        id,
        addrs: info.data.addrs().cloned().collect(),
    })
}

/// A master's periodic rendezvous publish, built under the engine lock and awaited
/// **off** it — publishing is a network round trip to the pkarr server, and the
/// engine mutex gates the reconcile loop and every IPC request.
pub struct RendezvousPublish {
    pub(crate) share_id: String,
    pub(crate) endpoint: Endpoint,
    pub(crate) seed: [u8; 32],
}

impl RendezvousPublish {
    /// Best-effort: a failed publish is retried on the next tick, and the previous
    /// record stays resolvable until its TTL expires.
    pub async fn run(self) {
        match tokio::time::timeout(TIMEOUT, publish(&self.endpoint, self.seed)).await {
            Ok(Ok(())) => {
                tracing::debug!("rendezvous: published address for share {}", self.share_id)
            }
            Ok(Err(e)) => tracing::warn!("rendezvous publish for {} failed: {e:#}", self.share_id),
            Err(_) => tracing::warn!("rendezvous publish for {} timed out", self.share_id),
        }
    }
}

/// An isolated node's attempt to find *any* live master via the rendezvous, and to
/// bootstrap both doc sync and presence gossip from it. Built under the engine lock,
/// run off it (see [`crate::engine::DocResync`] for why that split is mandatory).
pub struct RendezvousDial {
    pub(crate) share_id: String,
    pub(crate) endpoint: Endpoint,
    pub(crate) master_pub: [u8; 32],
    pub(crate) doc: Doc,
    pub(crate) presence: Option<iroh_gossip::api::GossipSender>,
}

impl RendezvousDial {
    /// Resolve a live master and bootstrap from it. Best-effort and idempotent: a
    /// share with no live master simply finds nothing and is retried next tick.
    pub async fn run(self) {
        let addr =
            match tokio::time::timeout(TIMEOUT, resolve(&self.endpoint, self.master_pub)).await {
                Ok(Ok(addr)) => addr,
                Ok(Err(e)) => {
                    // Expected while no master has published yet (e.g. every master in
                    // the pool predates this feature) — debug, not warn.
                    tracing::debug!(
                        "rendezvous lookup for {} found nothing: {e:#}",
                        self.share_id
                    );
                    return;
                }
                Err(_) => {
                    tracing::warn!("rendezvous lookup for {} timed out", self.share_id);
                    return;
                }
            };

        // The record is last-writer-wins across ALL masters, and we are a master too if
        // we hold the seed — so the address we just resolved is quite often our own.
        // Dialing ourselves is not merely useless: `SyncFinished` fires on failed syncs,
        // so it registers this device as its own peer and the member list grows a
        // phantom that no other node can see.
        if addr.id == self.endpoint.id() {
            tracing::debug!(
                "rendezvous: share {} resolved to ourselves (we published last); \
                 nothing to dial",
                self.share_id
            );
            return;
        }

        tracing::info!(
            "rendezvous: share {} has no reachable member; bootstrapping from master {}",
            self.share_id,
            addr.id.fmt_short(),
        );

        // Presence first: it's a queue hand-off to the gossip actor and returns
        // immediately, whereas start_sync dials.
        if let Some(sender) = self.presence {
            crate::presence::PresenceRejoin::new(sender, vec![addr.id])
                .join()
                .await;
        }
        if let Err(e) = self.doc.start_sync(vec![addr]).await {
            tracing::warn!("rendezvous doc sync for {} failed: {e:#}", self.share_id);
        }
    }
}
