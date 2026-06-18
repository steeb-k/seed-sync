//! Local persistence of share registry + anti-rollback watermarks (SQLite via
//! rusqlite). The iroh blob/doc stores persist their own data alongside; this DB
//! records which shares exist, where they live on disk, and the highest manifest
//! seqno accepted (so a restart can't be tricked into accepting an old manifest).
//!
//! Security note: the master key string (which contains the signing seed) is
//! currently stored here in the data directory, same protection level as the
//! iroh `node.key` that already lives there. Moving the seed into the OS keystore
//! (Windows DPAPI / Secret Service) is a planned hardening pass.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

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

    /// Remove a share row.
    pub fn remove_share(&self, share_id: &str) -> anyhow::Result<()> {
        self.lock()
            .execute("DELETE FROM shares WHERE share_id=?1", [share_id])?;
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
}
