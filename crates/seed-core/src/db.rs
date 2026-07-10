//! Local persistence of share registry + anti-rollback watermarks (SQLite via
//! rusqlite). The iroh blob/doc stores persist their own data alongside; this DB
//! records which shares exist, where they live on disk, and the highest manifest
//! seqno accepted (so a restart can't be tricked into accepting an old manifest).
//!
//! Security note: the master key string (which contains the signing seed) is
//! currently stored here in the data directory, same protection level as the
//! iroh `node.key` that already lives there. Moving the seed into the OS keystore
//! (Windows DPAPI / Secret Service) is a planned hardening pass.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};

/// A persisted share row.
#[derive(Debug, Clone)]
pub struct ShareRecord {
    pub share_id: String,
    /// Encoded share key (master `seedm…` or viewer `seedv…`).
    pub key: String,
    pub folder: String,
    pub role_master: bool,
    pub ignore: Vec<String>,
    pub last_seqno: u64,
    pub paused: bool,
    /// True when the master seed lives in the OS keystore and `key` is the
    /// seedless viewer key. False means `key` is self-contained (a viewer key,
    /// or a full master key in the keystore-unavailable fallback).
    pub seed_in_keyring: bool,
    /// Cheap (path,size,mtime) folder signature at the last publish. Persisted so
    /// a restart can tell an unchanged master folder from a changed one and skip
    /// a needless re-import. 0 means "unknown / republish".
    pub quick_sig: u64,
}

/// One open "degraded peer" episode: the accrual state behind the long-term
/// unhealthy-peer notifications. `node_id` is the peer's *full* endpoint-id
/// string, or `""` for this device itself. Persisted so a 12-hour clock
/// survives daemon restarts; rows exist only while an episode is open (a peer
/// observed healthy deletes its row).
#[derive(Debug, Clone, Default)]
pub struct PeerHealthRow {
    pub share_id: String,
    pub node_id: String,
    /// Unix secs the current *online* degraded spell began; 0 while the peer is
    /// offline (accrual paused, see `accum_secs`).
    pub degraded_since: i64,
    /// Degraded seconds accrued by earlier online spells of this episode
    /// (offline gaps pause the clock rather than resetting it).
    pub accum_secs: i64,
    /// Unix secs the last notification for this episode fired; 0 = none yet.
    pub last_notified_at: i64,
    /// Peer's last self-reported sync percent (display).
    pub last_percent: u8,
    /// Unix secs the peer was last heard from (staleness cleanup).
    pub last_seen: i64,
}

/// One remembered member identity: the last-known display name (and role) of a
/// share member, keyed by its *full* endpoint-id string. Persisted so the member
/// list keeps showing "who this was" across peer disconnects and daemon restarts
/// instead of falling back to a bare endpoint id. Rows are only ever superseded
/// (never expire): share membership is small and a stale name still beats a key
/// address.
#[derive(Debug, Clone, Default)]
pub struct PeerNameRow {
    pub share_id: String,
    pub node_id: String,
    pub name: String,
    pub role_master: bool,
    /// Unix secs of the last evidence this member was alive (heard directly, or
    /// the timestamp of the doc member-record that named it).
    pub last_seen: i64,
    /// Unix secs this identity (name/role) was last confirmed. A doc
    /// member-record only supersedes a newer direct observation if its own
    /// timestamp is newer — see `PeerRoster::note_member_records`.
    pub updated: i64,
}

/// `Connection` is `Send` but not `Sync`; wrap it so the `Db` (and thus the
/// `Engine`) is `Sync`, which the daemon needs to share the engine across tasks.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open (creating if needed) the state DB at `path` and ensure the schema.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS shares (
                 share_id   TEXT PRIMARY KEY,
                 key        TEXT NOT NULL,
                 folder     TEXT NOT NULL,
                 role_master INTEGER NOT NULL,
                 ignore     TEXT NOT NULL DEFAULT '',
                 last_seqno INTEGER NOT NULL DEFAULT 0,
                 paused     INTEGER NOT NULL DEFAULT 0,
                 seed_in_keyring INTEGER NOT NULL DEFAULT 0,
                 quick_sig  INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS settings (
                 k TEXT PRIMARY KEY,
                 v TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sync_index (
                 share_id TEXT NOT NULL,
                 path     TEXT NOT NULL,
                 hash     BLOB NOT NULL,
                 PRIMARY KEY (share_id, path)
             );
             CREATE TABLE IF NOT EXISTS peer_health (
                 share_id         TEXT NOT NULL,
                 node_id          TEXT NOT NULL,
                 degraded_since   INTEGER NOT NULL DEFAULT 0,
                 accum_secs       INTEGER NOT NULL DEFAULT 0,
                 last_notified_at INTEGER NOT NULL DEFAULT 0,
                 last_percent     INTEGER NOT NULL DEFAULT 0,
                 last_seen        INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (share_id, node_id)
             );
             CREATE TABLE IF NOT EXISTS peer_names (
                 share_id    TEXT NOT NULL,
                 node_id     TEXT NOT NULL,
                 name        TEXT NOT NULL,
                 role_master INTEGER NOT NULL DEFAULT 0,
                 last_seen   INTEGER NOT NULL DEFAULT 0,
                 updated     INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (share_id, node_id)
             );",
        )?;
        // Migration for DBs created before `quick_sig` existed; the error when the
        // column is already present is expected and ignored.
        let _ = conn.execute(
            "ALTER TABLE shares ADD COLUMN quick_sig INTEGER NOT NULL DEFAULT 0",
            [],
        );
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("db mutex poisoned")
    }

    /// Insert or replace a share row (used on create/add).
    pub fn upsert_share(&self, r: &ShareRecord) -> anyhow::Result<()> {
        self.lock().execute(
            "INSERT INTO shares (share_id, key, folder, role_master, ignore, last_seqno, paused, seed_in_keyring, quick_sig)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(share_id) DO UPDATE SET
                 key=excluded.key, folder=excluded.folder, role_master=excluded.role_master,
                 ignore=excluded.ignore, last_seqno=excluded.last_seqno, paused=excluded.paused,
                 seed_in_keyring=excluded.seed_in_keyring, quick_sig=excluded.quick_sig",
            rusqlite::params![
                r.share_id,
                r.key,
                r.folder,
                r.role_master as i64,
                r.ignore.join("\n"),
                r.last_seqno as i64,
                r.paused as i64,
                r.seed_in_keyring as i64,
                r.quick_sig as i64,
            ],
        )?;
        Ok(())
    }

    /// Persist the anti-rollback seqno watermark for a share.
    pub fn set_seqno(&self, share_id: &str, seqno: u64) -> anyhow::Result<()> {
        self.lock().execute(
            "UPDATE shares SET last_seqno=?2 WHERE share_id=?1",
            rusqlite::params![share_id, seqno as i64],
        )?;
        Ok(())
    }

    /// Persist the folder change-signature for a share (after a publish).
    pub fn set_quick_sig(&self, share_id: &str, quick_sig: u64) -> anyhow::Result<()> {
        self.lock().execute(
            "UPDATE shares SET quick_sig=?2 WHERE share_id=?1",
            rusqlite::params![share_id, quick_sig as i64],
        )?;
        Ok(())
    }

    /// Persist the paused flag for a share.
    pub fn set_paused(&self, share_id: &str, paused: bool) -> anyhow::Result<()> {
        self.lock().execute(
            "UPDATE shares SET paused=?2 WHERE share_id=?1",
            rusqlite::params![share_id, paused as i64],
        )?;
        Ok(())
    }

    /// Read a global key/value setting, or `None` if unset.
    pub fn get_setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.lock();
        let v = conn
            .query_row("SELECT v FROM settings WHERE k=?1", [key], |row| row.get(0))
            .optional()?;
        Ok(v)
    }

    /// Insert or update a global key/value setting.
    pub fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.lock().execute(
            "INSERT INTO settings (k, v) VALUES (?1, ?2)
             ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Remove a share row (and its per-path sync index, peer-health episodes,
    /// and remembered member names).
    pub fn remove_share(&self, share_id: &str) -> anyhow::Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM shares WHERE share_id=?1", [share_id])?;
        conn.execute("DELETE FROM sync_index WHERE share_id=?1", [share_id])?;
        conn.execute("DELETE FROM peer_health WHERE share_id=?1", [share_id])?;
        conn.execute("DELETE FROM peer_names WHERE share_id=?1", [share_id])?;
        Ok(())
    }

    /// Load the remembered member identities of one share (for prepopulating a
    /// fresh roster on share open, so the member list names offline members
    /// immediately after a restart).
    pub fn load_peer_names(&self, share_id: &str) -> anyhow::Result<Vec<PeerNameRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT share_id, node_id, name, role_master, last_seen, updated
             FROM peer_names WHERE share_id=?1",
        )?;
        let rows = stmt.query_map([share_id], |row| {
            Ok(PeerNameRow {
                share_id: row.get(0)?,
                node_id: row.get(1)?,
                name: row.get(2)?,
                role_master: row.get::<_, i64>(3)? != 0,
                last_seen: row.get(4)?,
                updated: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Insert or update one remembered member identity.
    pub fn upsert_peer_name(&self, r: &PeerNameRow) -> anyhow::Result<()> {
        self.lock().execute(
            "INSERT INTO peer_names (share_id, node_id, name, role_master, last_seen, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(share_id, node_id) DO UPDATE SET
                 name=excluded.name, role_master=excluded.role_master,
                 last_seen=excluded.last_seen, updated=excluded.updated",
            rusqlite::params![
                r.share_id,
                r.node_id,
                r.name,
                r.role_master as i64,
                r.last_seen,
                r.updated,
            ],
        )?;
        Ok(())
    }

    /// Load every open peer-health episode (all shares; filtered by the caller).
    pub fn load_peer_health(&self) -> anyhow::Result<Vec<PeerHealthRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT share_id, node_id, degraded_since, accum_secs, last_notified_at,
                    last_percent, last_seen
             FROM peer_health",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PeerHealthRow {
                share_id: row.get(0)?,
                node_id: row.get(1)?,
                degraded_since: row.get(2)?,
                accum_secs: row.get(3)?,
                last_notified_at: row.get(4)?,
                last_percent: row.get::<_, i64>(5)? as u8,
                last_seen: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Insert or update one peer-health episode row.
    pub fn upsert_peer_health(&self, r: &PeerHealthRow) -> anyhow::Result<()> {
        self.lock().execute(
            "INSERT INTO peer_health (share_id, node_id, degraded_since, accum_secs,
                                      last_notified_at, last_percent, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(share_id, node_id) DO UPDATE SET
                 degraded_since=excluded.degraded_since, accum_secs=excluded.accum_secs,
                 last_notified_at=excluded.last_notified_at,
                 last_percent=excluded.last_percent, last_seen=excluded.last_seen",
            rusqlite::params![
                r.share_id,
                r.node_id,
                r.degraded_since,
                r.accum_secs,
                r.last_notified_at,
                r.last_percent as i64,
                r.last_seen,
            ],
        )?;
        Ok(())
    }

    /// Close one peer-health episode (peer observed healthy / episode expired).
    pub fn delete_peer_health(&self, share_id: &str, node_id: &str) -> anyhow::Result<()> {
        self.lock().execute(
            "DELETE FROM peer_health WHERE share_id=?1 AND node_id=?2",
            rusqlite::params![share_id, node_id],
        )?;
        Ok(())
    }

    /// Load the per-path sync index (base state: `path -> last-reconciled hash`)
    /// for a share. Used by the unified reconcile to tell a new local add from a
    /// remotely-deleted file, and a local delete from a not-yet-materialized add.
    pub fn get_index(&self, share_id: &str) -> anyhow::Result<HashMap<String, Vec<u8>>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT path, hash FROM sync_index WHERE share_id=?1")?;
        let rows = stmt.query_map([share_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
    }

    /// Record (insert or replace) the reconciled hash for one path.
    pub fn set_index_entry(&self, share_id: &str, path: &str, hash: &[u8]) -> anyhow::Result<()> {
        self.lock().execute(
            "INSERT INTO sync_index (share_id, path, hash) VALUES (?1, ?2, ?3)
             ON CONFLICT(share_id, path) DO UPDATE SET hash=excluded.hash",
            rusqlite::params![share_id, path, hash],
        )?;
        Ok(())
    }

    /// Drop one path from a share's sync index (after a delete reconciled).
    pub fn del_index_entry(&self, share_id: &str, path: &str) -> anyhow::Result<()> {
        self.lock().execute(
            "DELETE FROM sync_index WHERE share_id=?1 AND path=?2",
            rusqlite::params![share_id, path],
        )?;
        Ok(())
    }

    /// Load all persisted shares (for reload on startup).
    pub fn load_all(&self) -> anyhow::Result<Vec<ShareRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT share_id, key, folder, role_master, ignore, last_seqno, paused, seed_in_keyring, quick_sig FROM shares",
        )?;
        let rows = stmt.query_map([], |row| {
            let ignore_str: String = row.get(4)?;
            Ok(ShareRecord {
                share_id: row.get(0)?,
                key: row.get(1)?,
                folder: row.get(2)?,
                role_master: row.get::<_, i64>(3)? != 0,
                ignore: if ignore_str.is_empty() {
                    Vec::new()
                } else {
                    ignore_str.split('\n').map(String::from).collect()
                },
                last_seqno: row.get::<_, i64>(5)? as u64,
                paused: row.get::<_, i64>(6)? != 0,
                seed_in_keyring: row.get::<_, i64>(7)? != 0,
                quick_sig: row.get::<_, i64>(8)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_share_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        let rec = ShareRecord {
            share_id: "abc".into(),
            key: "seedv1xyz".into(),
            folder: "/tmp/x".into(),
            role_master: false,
            ignore: vec!["*.tmp".into(), "node_modules".into()],
            last_seqno: 3,
            paused: false,
            seed_in_keyring: false,
            quick_sig: 0,
        };
        db.upsert_share(&rec).unwrap();
        db.set_seqno("abc", 7).unwrap();
        db.set_paused("abc", true).unwrap();
        db.set_quick_sig("abc", 12345).unwrap();

        let all = db.load_all().unwrap();
        assert_eq!(all.len(), 1);
        let r = &all[0];
        assert_eq!(r.share_id, "abc");
        assert_eq!(r.ignore, vec!["*.tmp", "node_modules"]);
        assert_eq!(r.last_seqno, 7);
        assert!(r.paused);
        assert_eq!(r.quick_sig, 12345);

        db.remove_share("abc").unwrap();
        assert!(db.load_all().unwrap().is_empty());
    }

    #[test]
    fn peer_health_roundtrip_and_share_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        let row = PeerHealthRow {
            share_id: "s1".into(),
            node_id: "peerA".into(),
            degraded_since: 1000,
            accum_secs: 50,
            last_notified_at: 0,
            last_percent: 73,
            last_seen: 1100,
        };
        db.upsert_peer_health(&row).unwrap();
        // Self row for the same share, and a row for another share.
        db.upsert_peer_health(&PeerHealthRow {
            share_id: "s1".into(),
            node_id: "".into(),
            ..row.clone()
        })
        .unwrap();
        db.upsert_peer_health(&PeerHealthRow {
            share_id: "s2".into(),
            node_id: "peerB".into(),
            ..row.clone()
        })
        .unwrap();

        let all = db.load_peer_health().unwrap();
        assert_eq!(all.len(), 3);
        let got = all
            .iter()
            .find(|r| r.share_id == "s1" && r.node_id == "peerA")
            .unwrap();
        assert_eq!(
            (
                got.degraded_since,
                got.accum_secs,
                got.last_percent,
                got.last_seen
            ),
            (1000, 50, 73, 1100)
        );

        // Update accrues in place (upsert, not duplicate).
        db.upsert_peer_health(&PeerHealthRow {
            accum_secs: 500,
            ..row.clone()
        })
        .unwrap();
        assert_eq!(db.load_peer_health().unwrap().len(), 3);

        // Closing one episode leaves the others.
        db.delete_peer_health("s1", "peerA").unwrap();
        assert_eq!(db.load_peer_health().unwrap().len(), 2);

        // Removing a share drops all its episodes.
        db.remove_share("s1").unwrap();
        let rest = db.load_peer_health().unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].share_id, "s2");
    }

    #[test]
    fn peer_names_roundtrip_and_share_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        let row = PeerNameRow {
            share_id: "s1".into(),
            node_id: "peerA".into(),
            name: "Laptop".into(),
            role_master: true,
            last_seen: 1000,
            updated: 1000,
        };
        db.upsert_peer_name(&row).unwrap();
        db.upsert_peer_name(&PeerNameRow {
            share_id: "s2".into(),
            ..row.clone()
        })
        .unwrap();

        // Per-share load sees only its own rows.
        let s1 = db.load_peer_names("s1").unwrap();
        assert_eq!(s1.len(), 1);
        assert_eq!(s1[0].name, "Laptop");
        assert!(s1[0].role_master);
        assert_eq!((s1[0].last_seen, s1[0].updated), (1000, 1000));

        // Upsert supersedes in place.
        db.upsert_peer_name(&PeerNameRow {
            name: "Laptop (renamed)".into(),
            role_master: false,
            updated: 2000,
            ..row.clone()
        })
        .unwrap();
        let s1 = db.load_peer_names("s1").unwrap();
        assert_eq!(s1.len(), 1);
        assert_eq!(s1[0].name, "Laptop (renamed)");
        assert!(!s1[0].role_master);
        assert_eq!(s1[0].updated, 2000);

        // Removing a share drops its rows, leaving other shares'.
        db.remove_share("s1").unwrap();
        assert!(db.load_peer_names("s1").unwrap().is_empty());
        assert_eq!(db.load_peer_names("s2").unwrap().len(), 1);
    }

    #[test]
    fn settings_get_set() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("state.db")).unwrap();
        assert_eq!(db.get_setting("device_name").unwrap(), None);
        db.set_setting("device_name", "Desktop").unwrap();
        assert_eq!(
            db.get_setting("device_name").unwrap(),
            Some("Desktop".into())
        );
        db.set_setting("device_name", "Laptop").unwrap();
        assert_eq!(
            db.get_setting("device_name").unwrap(),
            Some("Laptop".into())
        );
    }
}
