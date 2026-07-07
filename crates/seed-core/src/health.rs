//! Long-term peer-health tracking: the episode state machine behind the
//! "member unhealthy for 12+ hours" notifications.
//!
//! A peer (or this device itself) is **degraded** while it is *online but not
//! fully synced* — self-reported percent < 100, or holding a manifest
//! fingerprint that disagrees with the master-majority consensus. Plain offline
//! never starts the clock. Each open episode is persisted (`peer_health` table)
//! so the 12-hour timer survives daemon restarts, with **pause-not-reset**
//! semantics: a peer that drops offline mid-episode keeps its accrued time and
//! resumes the clock when it returns — it can't dodge the alert by bouncing —
//! while one continuously offline past [`HealthPolicy::offline_reset_secs`]
//! has its episode expired.
//!
//! The engine calls [`observe`] per (share, member) each detector pass (see
//! `Engine::health_alerts`); everything here is synchronous and cheap, safe
//! under a brief engine lock. DB writes happen only while an episode is open.

use std::collections::HashMap;

use crate::db::{Db, PeerHealthRow};

/// Detection/notification thresholds. Production defaults are hours; tests and
/// soaks shrink them to seconds via [`HealthPolicy::from_env`] (spawned
/// daemons) or `Engine::set_health_policy` (in-process).
#[derive(Clone, Copy, Debug)]
pub struct HealthPolicy {
    /// Continuous (online) degraded time before the first alert. 12 h.
    pub unhealthy_after_secs: i64,
    /// Re-alert cadence while still unhealthy. 8 h ≈ 2–3 alerts/day.
    pub renotify_secs: i64,
    /// A peer continuously offline this long has its episode expired (it isn't
    /// "degraded", it's gone; a fresh episode starts if it returns broken). 24 h.
    pub offline_reset_secs: i64,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        HealthPolicy {
            unhealthy_after_secs: 12 * 3600,
            renotify_secs: 8 * 3600,
            offline_reset_secs: 24 * 3600,
        }
    }
}

impl HealthPolicy {
    /// Defaults overridden by `SEED_HEALTH_UNHEALTHY_SECS`,
    /// `SEED_HEALTH_RENOTIFY_SECS`, `SEED_HEALTH_OFFLINE_RESET_SECS` — the knob
    /// integration tests and soaks use to run the 12-hour feature in seconds.
    pub fn from_env() -> Self {
        fn env_secs(key: &str, default: i64) -> i64 {
            std::env::var(key)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|v| *v > 0)
                .unwrap_or(default)
        }
        let d = Self::default();
        HealthPolicy {
            unhealthy_after_secs: env_secs("SEED_HEALTH_UNHEALTHY_SECS", d.unhealthy_after_secs),
            renotify_secs: env_secs("SEED_HEALTH_RENOTIFY_SECS", d.renotify_secs),
            offline_reset_secs: env_secs("SEED_HEALTH_OFFLINE_RESET_SECS", d.offline_reset_secs),
        }
    }
}

/// What one member looks like this detector pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    /// Online and not fully synced (percent < 100 or fingerprint disagrees).
    OnlineDegraded,
    /// Online and fully caught up.
    OnlineHealthy,
    /// Not currently heard from (accrual pauses; long enough silence expires
    /// the episode).
    Offline,
}

/// A due notification produced by [`observe`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackEvent {
    /// Unhealthy past the threshold (first alert or a renotify): total accrued
    /// degraded seconds.
    Degraded(i64),
    /// Was alerted about, now observed fully healthy again.
    Recovered,
}

/// Episode store: the in-memory mirror of the `peer_health` table, keyed by
/// `(share_id, node_id)` with `node_id == ""` meaning this device itself.
pub type Tracks = HashMap<(String, String), PeerHealthRow>;

/// Load persisted episodes at engine start. An episode that was mid-accrual
/// when the previous daemon stopped has its open spell folded into
/// `accum_secs` up to `last_seen` (the last written observation) and is left
/// paused — the observer can't know what happened while it was down, so it
/// under-counts by at most one detector interval rather than counting its own
/// downtime as the peer's degraded time.
pub fn load_tracks(db: &Db) -> Tracks {
    let mut tracks = Tracks::new();
    for mut row in db.load_peer_health().unwrap_or_default() {
        if row.degraded_since > 0 {
            row.accum_secs += (row.last_seen - row.degraded_since).max(0);
            row.degraded_since = 0;
        }
        tracks.insert((row.share_id.clone(), row.node_id.clone()), row);
    }
    tracks
}

/// Total degraded seconds accrued by an episode as of `now`.
pub fn accrued(row: &PeerHealthRow, now: i64) -> i64 {
    row.accum_secs
        + if row.degraded_since > 0 {
            (now - row.degraded_since).max(0)
        } else {
            0
        }
}

/// Advance one member's episode state machine and return a due notification,
/// if any. Persists transitions (and refreshes the open row so a restart loses
/// at most one interval); a member with no open episode and a healthy/offline
/// observation costs nothing.
pub fn observe(
    tracks: &mut Tracks,
    db: &Db,
    policy: &HealthPolicy,
    now: i64,
    share_id: &str,
    node_id: &str,
    obs: Observation,
    percent: u8,
) -> Option<TrackEvent> {
    let key = (share_id.to_string(), node_id.to_string());
    match obs {
        Observation::OnlineDegraded => {
            let row = tracks.entry(key).or_insert_with(|| PeerHealthRow {
                share_id: share_id.to_string(),
                node_id: node_id.to_string(),
                ..Default::default()
            });
            if row.degraded_since == 0 {
                row.degraded_since = now; // open (or resume) the accrual spell
            }
            row.last_percent = percent;
            row.last_seen = now;
            let total = accrued(row, now);
            let due = total >= policy.unhealthy_after_secs
                && (row.last_notified_at == 0
                    || now - row.last_notified_at >= policy.renotify_secs);
            if due {
                row.last_notified_at = now;
            }
            let _ = db.upsert_peer_health(row);
            due.then_some(TrackEvent::Degraded(total))
        }
        Observation::OnlineHealthy => {
            let row = tracks.remove(&key)?;
            let _ = db.delete_peer_health(share_id, node_id);
            (row.last_notified_at > 0).then_some(TrackEvent::Recovered)
        }
        Observation::Offline => {
            let row = tracks.get_mut(&key)?;
            if row.degraded_since > 0 {
                // Pause: bank the online spell up to the last actual sighting.
                row.accum_secs += (row.last_seen - row.degraded_since).max(0);
                row.degraded_since = 0;
                let _ = db.upsert_peer_health(row);
            }
            if now - row.last_seen > policy.offline_reset_secs {
                tracks.remove(&key);
                let _ = db.delete_peer_health(share_id, node_id);
            }
            None
        }
    }
}

/// Strict-majority fingerprint among master votes, or `None` on tie/empty.
/// Masters are the trust root, so only their fingerprints vote; requiring a
/// strict majority means a 1-vs-1 master split attributes nobody (both sides
/// self-alert instead) — misattributing the healthy node is worse than not
/// attributing at all.
pub fn consensus_fp(votes: &[u64]) -> Option<u64> {
    let mut counts: HashMap<u64, usize> = HashMap::new();
    for v in votes {
        *counts.entry(*v).or_default() += 1;
    }
    counts
        .into_iter()
        .find(|(_, n)| *n * 2 > votes.len())
        .map(|(fp, _)| fp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    fn fast_policy() -> HealthPolicy {
        HealthPolicy {
            unhealthy_after_secs: 100,
            renotify_secs: 50,
            offline_reset_secs: 1000,
        }
    }

    #[test]
    fn consensus_requires_strict_majority() {
        assert_eq!(consensus_fp(&[]), None);
        assert_eq!(consensus_fp(&[7]), Some(7));
        assert_eq!(consensus_fp(&[7, 7, 9]), Some(7));
        assert_eq!(consensus_fp(&[7, 9]), None, "1-vs-1 must not attribute");
        assert_eq!(
            consensus_fp(&[7, 7, 9, 9]),
            None,
            "2-vs-2 must not attribute"
        );
    }

    /// The 12h clock: no alert before the threshold, first alert at it,
    /// renotify on the cadence, recovery clears and reports once.
    /// (Epochs start at 1000: unix `now` is never 0, which is the "not
    /// accruing" sentinel in `degraded_since`.)
    #[test]
    fn alert_renotify_recover_cycle() {
        let (_d, db) = test_db();
        let p = fast_policy();
        let mut t = Tracks::new();
        let mut at =
            |tracks: &mut Tracks, now, obs| observe(tracks, &db, &p, now, "s", "peer", obs, 40);

        assert_eq!(at(&mut t, 1000, Observation::OnlineDegraded), None);
        assert_eq!(at(&mut t, 1099, Observation::OnlineDegraded), None);
        assert_eq!(
            at(&mut t, 1100, Observation::OnlineDegraded),
            Some(TrackEvent::Degraded(100)),
            "first alert exactly at the threshold"
        );
        assert_eq!(
            at(&mut t, 1120, Observation::OnlineDegraded),
            None,
            "renotify not due"
        );
        assert_eq!(
            at(&mut t, 1150, Observation::OnlineDegraded),
            Some(TrackEvent::Degraded(150)),
            "renotify on the cadence"
        );
        assert_eq!(
            at(&mut t, 1160, Observation::OnlineHealthy),
            Some(TrackEvent::Recovered),
            "recovery after an alert must announce"
        );
        assert!(t.is_empty(), "episode fully cleared");
        assert_eq!(
            at(&mut t, 1170, Observation::OnlineHealthy),
            None,
            "healthy with no episode is free"
        );
    }

    /// Pause-not-reset: offline gaps keep accrued time; the clock can't be
    /// dodged by bouncing. Continuous offline past the cap expires the episode.
    #[test]
    fn offline_pauses_then_expires() {
        let (_d, db) = test_db();
        let p = fast_policy();
        let mut t = Tracks::new();

        // 60s degraded, then a gap, then 40s more → crosses 100 despite the gap.
        observe(
            &mut t,
            &db,
            &p,
            1000,
            "s",
            "x",
            Observation::OnlineDegraded,
            10,
        );
        observe(
            &mut t,
            &db,
            &p,
            1060,
            "s",
            "x",
            Observation::OnlineDegraded,
            10,
        );
        assert_eq!(
            observe(&mut t, &db, &p, 1070, "s", "x", Observation::Offline, 0),
            None
        );
        // Long gap (but < offline_reset): accrual resumes where it left off.
        assert_eq!(
            observe(
                &mut t,
                &db,
                &p,
                1500,
                "s",
                "x",
                Observation::OnlineDegraded,
                10
            ),
            None,
            "60 accrued + 0 this spell"
        );
        assert_eq!(
            observe(
                &mut t,
                &db,
                &p,
                1540,
                "s",
                "x",
                Observation::OnlineDegraded,
                10
            ),
            Some(TrackEvent::Degraded(100)),
            "banked 60s + 40s new spell crosses the threshold"
        );

        // A recovered-without-alert episode just clears silently… (fresh peer)
        observe(
            &mut t,
            &db,
            &p,
            1000,
            "s",
            "y",
            Observation::OnlineDegraded,
            10,
        );
        assert_eq!(
            observe(
                &mut t,
                &db,
                &p,
                1010,
                "s",
                "y",
                Observation::OnlineHealthy,
                100
            ),
            None,
            "no alert fired → no recovery announcement"
        );

        // …and continuous offline past the cap expires an episode.
        observe(
            &mut t,
            &db,
            &p,
            1000,
            "s",
            "z",
            Observation::OnlineDegraded,
            10,
        );
        observe(&mut t, &db, &p, 1010, "s", "z", Observation::Offline, 0);
        observe(&mut t, &db, &p, 3000, "s", "z", Observation::Offline, 0);
        assert!(
            !t.contains_key(&("s".into(), "z".into())),
            "stale episode expired"
        );
    }

    /// Restart: an open spell is banked up to `last_seen` and left paused, so
    /// observer downtime never counts as peer degraded time.
    #[test]
    fn load_banks_open_spell() {
        let (_d, db) = test_db();
        let p = fast_policy();
        {
            let mut t = Tracks::new();
            observe(
                &mut t,
                &db,
                &p,
                1000,
                "s",
                "x",
                Observation::OnlineDegraded,
                10,
            );
            observe(
                &mut t,
                &db,
                &p,
                1080,
                "s",
                "x",
                Observation::OnlineDegraded,
                10,
            );
        } // "daemon stops" with an open spell persisted (last_seen = 1080)

        let mut t = load_tracks(&db);
        let row = t.get(&("s".into(), "x".into())).expect("episode survived");
        assert_eq!(row.accum_secs, 80, "open spell banked to last_seen");
        assert_eq!(row.degraded_since, 0, "left paused");
        // 20 more degraded seconds after the restart cross the threshold, even
        // though wall-clock 'now' jumped far ahead while the daemon was down.
        observe(
            &mut t,
            &db,
            &p,
            5000,
            "s",
            "x",
            Observation::OnlineDegraded,
            10,
        );
        assert_eq!(
            observe(
                &mut t,
                &db,
                &p,
                5020,
                "s",
                "x",
                Observation::OnlineDegraded,
                10
            ),
            Some(TrackEvent::Degraded(100))
        );
    }
}
