//! User-configured **custom relay servers** (self-hosted iroh relays), with an
//! optional per-relay auth token and a policy for how they combine with the
//! public number-0 relays.
//!
//! Two policies:
//! - [`RelayPolicy::Preferred`] — the endpoint runs on the custom relays alone;
//!   if none of them is reachable the [`relay_watchdog`] *adds* the public n0
//!   relays as a fallback, and removes them again once a custom relay is back.
//!   Best of both: your relay carries traffic whenever it's up, but a dead
//!   relay never strands you.
//! - [`RelayPolicy::Only`] — custom relays only, never fall back. For setups
//!   that must not touch third-party infrastructure.
//!
//! With no custom relays configured the endpoint uses the iroh defaults and the
//! policy is irrelevant.
//!
//! The module also provides [`PreferMyRelaySelector`], a [`PathSelector`] that
//! mirrors iroh's default biased-RTT policy but slots relay paths through one
//! of the *user's* relays above relay paths through anything else. Direct
//! (hole-punched) paths still always win over any relay.
//!
//! Settings are per-device (`settings["relay_settings"]` in `state.db`) and are
//! **not** distributed to share members: every member that should use the relay
//! has to configure it (a relay with a token rejects clients that don't have
//! it, which would also break relay-assisted hole-punching with unconfigured
//! peers).
//!
//! The access token gates only the **relay connection** — the WebSocket path
//! that carries relayed traffic and hole-punching coordination. A relay's QUIC
//! address-discovery service (STUN-like "what's my public address", UDP 7842)
//! and its HTTPS latency endpoint are unauthenticated by design (verified in
//! iroh-relay 1.0.0), so even a token-less client still gets address discovery
//! from the server; it just can't have traffic relayed through it. This is why
//! peers with a working direct path keep connecting even when the token is
//! missing — and why [`probe_relay`] reports that case distinctly.

use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};

use anyhow::{bail, Context, Result};
use iroh::{
    endpoint::transports::{FourTuple, PathSelection, PathSelectionContext, PathSelector},
    Endpoint, RelayConfig, RelayMap, RelayMode, RelayUrl,
};

pub use seed_ipc::{RelayPolicy, RelayServer, RelaySettings};

/// Key of the persisted settings in the `settings` table.
const SETTINGS_KEY: &str = "relay_settings";

/// Parses the configured servers into iroh [`RelayConfig`]s (with tokens
/// attached), validating as it goes. An empty list is valid and means
/// "use the defaults".
pub fn relay_configs(settings: &RelaySettings) -> Result<Vec<RelayConfig>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(settings.servers.len());
    for s in &settings.servers {
        let url: RelayUrl = s
            .url
            .parse()
            .with_context(|| format!("invalid relay URL {:?}", s.url))?;
        if !seen.insert(url.clone()) {
            bail!("duplicate relay URL {url}");
        }
        let mut cfg = RelayConfig::from(url);
        if let Some(token) = &s.token {
            // The token travels in an HTTP header; reject values that can't
            // (control chars / non-ASCII) up front rather than failing every
            // connection attempt later.
            if token.is_empty() || !token.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
                bail!(
                    "relay token for {} must be non-empty printable ASCII without spaces",
                    s.url
                );
            }
            cfg = cfg.with_auth_token(token.clone());
        }
        out.push(cfg);
    }
    Ok(out)
}

/// The parsed URLs of the configured servers (order preserved).
pub fn relay_urls(settings: &RelaySettings) -> Result<Vec<RelayUrl>> {
    Ok(relay_configs(settings)?
        .into_iter()
        .map(|c| c.url)
        .collect())
}

/// The public iroh relays used as the default map and as the fallback set.
pub(crate) fn default_relay_configs() -> Vec<Arc<RelayConfig>> {
    iroh::endpoint::default_relay_mode().relay_map().relays()
}

/// Load the persisted relay settings from the state DB. A missing or corrupt
/// value must never brick connectivity: it degrades to "no custom relays".
pub fn load_relay_settings(db: &crate::db::Db) -> RelaySettings {
    db.get_setting(SETTINGS_KEY)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// Persist the relay settings to the state DB.
pub fn save_relay_settings(db: &crate::db::Db, settings: &RelaySettings) -> Result<()> {
    let v = serde_json::to_string(settings).context("encode relay settings")?;
    db.set_setting(SETTINGS_KEY, &v)
}

/// The `relay_mode` for the endpoint builder: the custom relays when any are
/// configured (invalid settings degrade to the defaults with a warning).
pub(crate) fn relay_mode(settings: &RelaySettings) -> Option<RelayMode> {
    match relay_configs(settings) {
        Ok(configs) if !configs.is_empty() => Some(RelayMode::Custom(RelayMap::from_iter(configs))),
        Ok(_) => None, // no custom relays; keep the preset defaults
        Err(e) => {
            tracing::warn!("ignoring invalid relay settings: {e:#}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Connectivity probe
// ---------------------------------------------------------------------------

/// Outcome of [`probe_relay`], shaped for the GUI's "Test connection" button.
#[derive(Debug, Clone)]
pub struct RelayProbeOutcome {
    pub ok: bool,
    pub detail: String,
}

/// Probe a relay server by binding a throwaway endpoint whose relay map is
/// pinned to ONLY this relay (no discovery services) and waiting for it to
/// come online — which exercises the real HTTPS/WebSocket relay connection
/// including the `Authorization: Bearer` token handshake.
///
/// When the relay connection does NOT come up, the endpoint's net-report tells
/// the two failure modes apart: its probes hit the relay's HTTPS latency
/// endpoint and QUIC address-discovery service, neither of which is
/// token-gated, and an entry only ever appears for a probe that got an answer.
/// So probe entries for our URL + no relay connection = the server is up but
/// refused us (missing/wrong access token); no entries = the server itself is
/// unreachable (wrong URL, down, firewalled).
pub async fn probe_relay(url: &str, token: Option<&str>, timeout: Duration) -> RelayProbeOutcome {
    use iroh::{unstable_net_report::Probe, Watcher as _};

    let settings = RelaySettings {
        servers: vec![RelayServer {
            url: url.to_string(),
            token: token.map(str::to_string),
        }],
        mode: RelayPolicy::Only,
    };
    let cfg = match relay_configs(&settings) {
        Ok(mut configs) => configs.remove(0),
        Err(e) => {
            return RelayProbeOutcome {
                ok: false,
                detail: format!("{e:#}"),
            }
        }
    };
    let relay_url = cfg.url.clone();
    let endpoint = match Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(RelayMode::Custom(RelayMap::from(cfg)))
        .bind()
        .await
    {
        Ok(ep) => ep,
        Err(e) => {
            return RelayProbeOutcome {
                ok: false,
                detail: format!("could not bind a probe endpoint: {e:#}"),
            }
        }
    };
    let started = std::time::Instant::now();
    let online = tokio::time::timeout(timeout, endpoint.online())
        .await
        .is_ok();
    if online {
        let elapsed = started.elapsed();
        endpoint.close().await;
        return RelayProbeOutcome {
            ok: true,
            detail: format!(
                "Connected in {:.1}s — this relay can carry traffic for this device",
                elapsed.as_secs_f64()
            ),
        };
    }

    // The relay connection never came up; net-report ran in parallel the whole
    // time, so its verdict on the token-less services is normally already in.
    // A short grace poll covers a slow first report.
    let mut watcher = endpoint.net_report();
    let grace = std::time::Instant::now() + Duration::from_secs(3);
    let (mut https, mut qad) = (false, false);
    loop {
        if let Some(report) = watcher.get() {
            for (probe, u, _latency) in report.relay_latency.iter() {
                if *u == relay_url {
                    match probe {
                        Probe::Https => https = true,
                        _ => qad = true, // QadIpv4 / QadIpv6
                    }
                }
            }
        }
        if https || qad || std::time::Instant::now() > grace {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    endpoint.close().await;

    let detail = if qad || https {
        let service = if qad {
            "its address discovery answers"
        } else {
            "its HTTPS endpoint answers"
        };
        match token {
            Some(_) => format!(
                "The server is up ({service}) but refused the relay connection — \
                 the access token was not accepted"
            ),
            None => "Discovery only. No relay connections".to_string(),
        }
    } else {
        format!(
            "No response from the server within {}s — check the URL and that the \
             relay is running and reachable",
            timeout.as_secs()
        )
    };
    RelayProbeOutcome { ok: false, detail }
}

// ---------------------------------------------------------------------------
// Fallback watchdog
// ---------------------------------------------------------------------------

/// Custom-relay fallback watchdog. Only acts when custom relays are configured
/// with [`RelayPolicy::Preferred`]: after ~30s without a connection to any of
/// the user's relays it *adds* the public iroh relays to the live map, and it
/// removes them again as soon as a custom relay is connected (the endpoint
/// re-probes every relay in the map on its ~20-26s net-report cycle, and the
/// user's relay wins the home-relay pick once it answers again — the path
/// selector prefers it regardless).
///
/// `force_fallback` is the escape hatch for a relay that reads *connected* but is
/// silently **blackholing** client↔client traffic — the exact shape of the
/// 2026-07 fleet isolation (known-issues #23), where every
/// member homed on a custom relay that answered its own ping/handshake while
/// forwarding nothing, so `is_connected()` stayed true and this watchdog never
/// engaged. The engine's partition self-heal sets this flag once a share has been
/// unable to reach any member for long enough; while set, the watchdog treats the
/// custom relay as unusable and adds the public relays regardless of
/// `is_connected()`. Adding relays never strands a node, so this is safe even if
/// the custom relay is actually fine — it just gains a second path. Honored only
/// in `Preferred` mode: `Only` means "never touch third-party infra", a choice a
/// blackhole does not override.
///
/// Runs detached for the endpoint's lifetime; the engine aborts it on shutdown.
pub(crate) async fn relay_watchdog(
    endpoint: Endpoint,
    settings: Arc<StdMutex<RelaySettings>>,
    fallback: Arc<AtomicBool>,
    force_fallback: Arc<AtomicBool>,
) {
    use iroh::Watcher as _;

    const TICK: Duration = Duration::from_secs(10);
    const MISSES_TO_FALL_BACK: u32 = 3;

    let mut status = endpoint.home_relay_status();
    let mut misses = 0u32;
    loop {
        tokio::time::sleep(TICK).await;
        let current = settings.lock().expect("relay settings poisoned").clone();
        let custom_urls: BTreeSet<RelayUrl> = match relay_urls(&current) {
            Ok(urls) if !urls.is_empty() && current.mode == RelayPolicy::Preferred => {
                urls.into_iter().collect()
            }
            _ => {
                misses = 0;
                continue;
            }
        };
        // A forced fallback (blackhole detected by the engine) counts as "no custom
        // relay reachable" even when the transport still reads connected.
        let forced = force_fallback.load(Ordering::Relaxed);
        let connected_custom = !forced
            && status
                .get()
                .iter()
                .any(|s| s.is_connected() && custom_urls.contains(s.url()));

        if connected_custom {
            misses = 0;
            if fallback.swap(false, Ordering::Relaxed) {
                for cfg in default_relay_configs() {
                    if !custom_urls.contains(&cfg.url) {
                        endpoint.remove_relay(&cfg.url).await;
                    }
                }
                tracing::info!("custom relay reachable again; leaving the public relays");
            }
        } else if !fallback.load(Ordering::Relaxed) {
            misses += 1;
            if misses >= MISSES_TO_FALL_BACK {
                for cfg in default_relay_configs() {
                    endpoint.insert_relay(cfg.url.clone(), cfg).await;
                }
                fallback.store(true, Ordering::Relaxed);
                if forced {
                    tracing::warn!(
                        "custom relay reads connected but a share can reach no member \
                         (suspected blackhole); adding the public relays as a fallback path"
                    );
                } else {
                    tracing::warn!(
                        "no custom relay reachable for {}s; falling back to the public relays",
                        TICK.as_secs() * MISSES_TO_FALL_BACK as u64
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Path selection
// ---------------------------------------------------------------------------

/// Shared, live-updatable set of relay URLs the selector should prefer. The
/// engine updates it when the user edits relay settings; the endpoint holds
/// the selector (and thus this handle) for its whole lifetime.
#[derive(Clone, Debug, Default)]
pub struct PreferredRelays(Arc<std::sync::RwLock<BTreeSet<RelayUrl>>>);

impl PreferredRelays {
    pub fn set(&self, urls: BTreeSet<RelayUrl>) {
        *self.0.write().expect("poisoned") = urls;
    }

    fn contains(&self, url: &RelayUrl) -> bool {
        self.0.read().expect("poisoned").contains(url)
    }
}

/// Mirrors iroh's default path selection (IPv6 3ms ahead of IPv4, lowest
/// biased RTT wins, 5ms stickiness against flapping) with one change: relay
/// paths are split into two tiers, and a path through one of the user's own
/// relays always beats a path through any other relay. Direct paths keep
/// beating every relay, so this only matters while traffic is relayed.
const IPV6_RTT_ADVANTAGE: Duration = Duration::from_millis(3);
const RTT_SWITCHING_MIN: Duration = Duration::from_millis(5);

/// Preference tier of a path: lower wins regardless of RTT. `0` = direct
/// (IP or custom transport), `1` = one of the user's relays, `2` = any other
/// relay. Mirrors iroh's Primary/Backup split with the relay tier divided.
fn path_tier(path: &FourTuple, preferred: &PreferredRelays) -> u8 {
    match path {
        FourTuple::Relay { url, .. } => {
            if preferred.contains(url) {
                1
            } else {
                2
            }
        }
        _ => 0,
    }
}

/// Biased RTT in nanoseconds: the tie-breaker within a tier.
fn biased_rtt(path: &FourTuple, rtt: Duration) -> i128 {
    let mut ns = rtt.as_nanos() as i128;
    if matches!(path, FourTuple::Ip { remote, .. } if remote.is_ipv6()) {
        ns -= IPV6_RTT_ADVANTAGE.as_nanos() as i128;
    }
    ns
}

/// Whether a path with key `best` should replace the current path with key
/// `current`: immediately across tiers, only on a clear RTT win within one.
fn should_switch(current: (u8, i128), best: (u8, i128)) -> bool {
    if best.0 != current.0 {
        best.0 < current.0
    } else {
        best.1 + RTT_SWITCHING_MIN.as_nanos() as i128 <= current.1
    }
}

#[derive(Debug)]
pub struct PreferMyRelaySelector {
    preferred: PreferredRelays,
}

impl PreferMyRelaySelector {
    pub fn new(preferred: PreferredRelays) -> Self {
        Self { preferred }
    }
}

impl PathSelector for PreferMyRelaySelector {
    fn select(&self, ctx: &PathSelectionContext<'_>) -> PathSelection {
        let current = ctx.current();
        let mut best: Option<(
            iroh::endpoint::transports::PathSelectionData<'_>,
            (u8, i128),
        )> = None;
        let mut current_key: Option<(u8, i128)> = None;

        for psd in ctx.paths() {
            let path = psd.network_path();
            // A path whose stats are gone was closed concurrently; skip it.
            let Some(stats) = psd.stats() else { continue };
            let key = (
                path_tier(path, &self.preferred),
                biased_rtt(path, stats.rtt),
            );
            if Some(path) == current && current_key.is_none_or(|c| key < c) {
                current_key = Some(key);
            }
            if best.as_ref().is_none_or(|(_, b)| key < *b) {
                best = Some((psd, key));
            }
        }

        let mut selection = PathSelection::none();
        let Some((best_psd, best_key)) = best else {
            return selection;
        };
        match current_key {
            // No current path (or no stats for it): take the best one.
            None => selection.set(&best_psd),
            Some(cur) if should_switch(cur, best_key) => selection.set(&best_psd),
            Some(_) => {}
        }
        selection
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn relay_path(url: &str) -> FourTuple {
        FourTuple::from_remote(iroh::endpoint::transports::Addr::Relay(
            url.parse().unwrap(),
            iroh::EndpointId::from_bytes(&[0u8; 32]).unwrap(),
        ))
    }

    fn ip_path(v6: bool) -> FourTuple {
        let addr: std::net::SocketAddr = if v6 {
            "[::1]:9000".parse().unwrap()
        } else {
            "127.0.0.1:9000".parse().unwrap()
        };
        FourTuple::from_remote(iroh::endpoint::transports::Addr::Ip(addr))
    }

    fn preferred(urls: &[&str]) -> PreferredRelays {
        let p = PreferredRelays::default();
        p.set(urls.iter().map(|u| u.parse().unwrap()).collect());
        p
    }

    #[test]
    fn my_relay_beats_faster_foreign_relay() {
        let p = preferred(&["https://mine.example.com"]);
        let mine = relay_path("https://mine.example.com");
        let n0 = relay_path("https://relay.iroh.link");
        // Even a much faster foreign relay stays in a worse tier.
        let mine_key = (path_tier(&mine, &p), biased_rtt(&mine, ms(80)));
        let n0_key = (path_tier(&n0, &p), biased_rtt(&n0, ms(5)));
        assert!(mine_key < n0_key);
        assert!(should_switch(n0_key, mine_key));
        // ...but a direct path still beats my relay.
        let direct_key = (
            path_tier(&ip_path(false), &p),
            biased_rtt(&ip_path(false), ms(200)),
        );
        assert!(direct_key < mine_key);
    }

    #[test]
    fn without_preferred_relays_matches_default_policy() {
        let p = preferred(&[]);
        let a = relay_path("https://a.example.com");
        let b = relay_path("https://b.example.com");
        // Same tier: RTT + stickiness decide.
        let a_key = (path_tier(&a, &p), biased_rtt(&a, ms(50)));
        let b_fast = (path_tier(&b, &p), biased_rtt(&b, ms(40)));
        let b_close = (path_tier(&b, &p), biased_rtt(&b, ms(47)));
        assert!(should_switch(a_key, b_fast), "5ms better switches");
        assert!(!should_switch(a_key, b_close), "3ms better sticks");
        // IPv6 gets its 3ms advantage over IPv4.
        assert!(biased_rtt(&ip_path(true), ms(10)) < biased_rtt(&ip_path(false), ms(9)));
    }

    #[test]
    fn live_update_of_preferred_set() {
        let p = preferred(&[]);
        let mine = relay_path("https://mine.example.com");
        assert_eq!(path_tier(&mine, &p), 2);
        p.set(["https://mine.example.com".parse().unwrap()].into());
        assert_eq!(path_tier(&mine, &p), 1);
    }

    #[test]
    fn settings_validation() {
        let ok = RelaySettings {
            servers: vec![RelayServer {
                url: "https://relay.example.com:8443".into(),
                token: Some("s3cret-Token_123".into()),
            }],
            mode: RelayPolicy::Preferred,
        };
        assert_eq!(relay_configs(&ok).unwrap().len(), 1);
        assert!(relay_configs(&ok).unwrap()[0].auth_token.is_some());

        for bad in [
            RelaySettings {
                servers: vec![RelayServer {
                    url: "not a url".into(),
                    token: None,
                }],
                ..Default::default()
            },
            RelaySettings {
                servers: vec![RelayServer {
                    url: "https://a.example.com".into(),
                    token: Some("has space".into()),
                }],
                ..Default::default()
            },
            RelaySettings {
                servers: vec![
                    RelayServer {
                        url: "https://a.example.com".into(),
                        token: None,
                    },
                    RelayServer {
                        url: "https://a.example.com".into(),
                        token: None,
                    },
                ],
                ..Default::default()
            },
        ] {
            assert!(relay_configs(&bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn settings_db_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("state.db")).unwrap();
        // Unset -> defaults.
        assert_eq!(load_relay_settings(&db), RelaySettings::default());
        let s = RelaySettings {
            servers: vec![RelayServer {
                url: "https://relay.example.com:8443".into(),
                token: Some("tok".into()),
            }],
            mode: RelayPolicy::Only,
        };
        save_relay_settings(&db, &s).unwrap();
        assert_eq!(load_relay_settings(&db), s);
        // Corrupt value degrades to defaults instead of erroring.
        db.set_setting(SETTINGS_KEY, "{not json").unwrap();
        assert_eq!(load_relay_settings(&db), RelaySettings::default());
    }
}
