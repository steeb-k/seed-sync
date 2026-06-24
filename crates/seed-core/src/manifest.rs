//! The signed manifest — SEED Sync's trust root.
//!
//! iroh-docs replicas are multi-writer by nature and enforce **no** application
//! permissions. We layer authority on top: the master holds an Ed25519 signing
//! key and publishes a [`SignedManifest`] describing the authoritative folder
//! state (a BLAKE3 merkle root over the file set), a strictly-increasing
//! `seqno` (anti-rollback), and an `expiry`. Viewers verify the signature
//! against the master public key pinned in their viewer key, reject stale or
//! expired manifests, and overwrite any local state that disagrees.
//!
//! The format is forward-compatible with future multi-master: `writers` and
//! `revoked` are reserved optional fields, gated behind `format`, so populating
//! them later is additive rather than a breaking change.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Current manifest format version. Bumped only for breaking changes; additive
/// fields (e.g. multi-master `writers`) do not require a bump.
pub const MANIFEST_FORMAT: u16 = 1;

/// One entry in the authoritative file set. `path` is a relative POSIX path
/// (forward slashes, NFC-normalized). A deletion is represented by `deleted = true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    /// BLAKE3 hash of the file content (zeroed for tombstones).
    #[serde(with = "serde_bytes")]
    pub hash: Vec<u8>,
    pub size: u64,
    pub deleted: bool,
}

/// The signable payload. Serialized canonically (sorted entries) before signing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u16,
    #[serde(with = "serde_bytes")]
    pub share_id: Vec<u8>,
    /// Strictly increasing across publishes; viewers reject `<= last_seen`.
    pub seqno: u64,
    /// Unix seconds; viewers reject manifests with `expiry < now`.
    pub expiry: i64,
    /// BLAKE3 merkle root over the sorted file set (see [`Manifest::compute_root`]).
    /// Redundant given `files`, but a compact version identifier and a
    /// self-consistency check.
    #[serde(with = "serde_bytes")]
    pub merkle_root: Vec<u8>,
    /// The authoritative live file set. A path's *absence* here means "deleted":
    /// viewers remove any local file not listed. This is the single source of
    /// truth the master signs.
    pub files: Vec<FileEntry>,
    /// Master-defined ignore globs, honored identically by all peers.
    pub ignore: Vec<String>,
    // --- reserved for future multi-master (parsed but unused at format 1) ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writers: Option<Vec<WriterEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked: Option<Vec<Vec<u8>>>,
}

/// Future multi-master: an authorized writer identity and its capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterEntry {
    #[serde(with = "serde_bytes")]
    pub pubkey: Vec<u8>,
    pub added_seq: u64,
}

/// A manifest plus the signer's public key and detached signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedManifest {
    /// Canonical CBOR encoding of the [`Manifest`] that was signed.
    #[serde(with = "serde_bytes")]
    pub manifest_cbor: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub signer: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub sig: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("unsupported manifest format {0}")]
    UnsupportedFormat(u16),
    #[error("signer is not the pinned master key")]
    WrongSigner,
    #[error("invalid signature")]
    BadSignature,
    #[error("stale manifest: seqno {got} <= last seen {last}")]
    StaleSeqno { got: u64, last: u64 },
    #[error("expired manifest: expiry {expiry} < now {now}")]
    Expired { expiry: i64, now: i64 },
    #[error("merkle root does not match file set")]
    RootMismatch,
    #[error("share id mismatch")]
    ShareIdMismatch,
    #[error("encoding: {0}")]
    Encoding(String),
}

impl Manifest {
    /// Build an unsigned manifest for `files`, computing the merkle root.
    pub fn new(
        share_id: Vec<u8>,
        seqno: u64,
        expiry: i64,
        mut files: Vec<FileEntry>,
        ignore: Vec<String>,
    ) -> Self {
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let merkle_root = Self::compute_root(&files);
        Self {
            format: MANIFEST_FORMAT,
            share_id,
            seqno,
            expiry,
            merkle_root,
            files,
            ignore,
            writers: None,
            revoked: None,
        }
    }

    /// Compute the BLAKE3 merkle root over a file set. Entries are sorted by path
    /// for a deterministic, order-independent root. Each entry contributes
    /// `path \0 hash size deleted` to a rolling hasher.
    pub fn compute_root(entries: &[FileEntry]) -> Vec<u8> {
        let mut sorted: Vec<&FileEntry> = entries.iter().collect();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let mut hasher = blake3::Hasher::new();
        for e in sorted {
            hasher.update(e.path.as_bytes());
            hasher.update(&[0u8]);
            hasher.update(&e.hash);
            hasher.update(&e.size.to_le_bytes());
            hasher.update(&[e.deleted as u8]);
        }
        hasher.finalize().as_bytes().to_vec()
    }

    fn to_canonical_cbor(&self) -> Result<Vec<u8>, ManifestError> {
        let mut buf = Vec::new();
        ciborium::into_writer(self, &mut buf)
            .map_err(|e| ManifestError::Encoding(e.to_string()))?;
        Ok(buf)
    }

    fn from_cbor(bytes: &[u8]) -> Result<Self, ManifestError> {
        ciborium::from_reader(bytes).map_err(|e| ManifestError::Encoding(e.to_string()))
    }
}

/// Sign a manifest with the master signing key, producing a [`SignedManifest`].
pub fn sign(manifest: &Manifest, key: &SigningKey) -> Result<SignedManifest, ManifestError> {
    let manifest_cbor = manifest.to_canonical_cbor()?;
    let sig: Signature = key.sign(&manifest_cbor);
    Ok(SignedManifest {
        manifest_cbor,
        signer: key.verifying_key().to_bytes().to_vec(),
        sig: sig.to_bytes().to_vec(),
    })
}

/// Parameters a viewer checks a signed manifest against.
pub struct VerifyContext<'a> {
    /// The master public key pinned in the viewer/master key string.
    pub pinned_master: &'a VerifyingKey,
    /// The share id this peer expects.
    pub expected_share_id: &'a [u8],
    /// Highest seqno previously accepted (persisted watermark). 0 if none.
    pub last_seqno: u64,
    /// Current unix time in seconds.
    pub now: i64,
}

/// Verify everything about a signed manifest *except* the anti-rollback seqno
/// check: the signer is the pinned master key, the signature is valid, the
/// format is supported, the share id matches, and it has not expired. Callers
/// that also need anti-rollback use [`verify`]; callers that want to re-apply
/// the current manifest (e.g. to revert local drift) use this and compare
/// seqno themselves.
pub fn verify_signed(
    signed: &SignedManifest,
    pinned_master: &VerifyingKey,
    expected_share_id: &[u8],
    now: i64,
) -> Result<Manifest, ManifestError> {
    // 1. Signer must be the pinned master key.
    if signed.signer != pinned_master.to_bytes() {
        return Err(ManifestError::WrongSigner);
    }
    // 2. Signature must be valid over the exact bytes that were signed.
    let sig = Signature::from_slice(&signed.sig).map_err(|_| ManifestError::BadSignature)?;
    pinned_master
        .verify(&signed.manifest_cbor, &sig)
        .map_err(|_| ManifestError::BadSignature)?;

    let manifest = Manifest::from_cbor(&signed.manifest_cbor)?;

    // 3. Format gate.
    if manifest.format > MANIFEST_FORMAT {
        return Err(ManifestError::UnsupportedFormat(manifest.format));
    }
    // 4. Share id must match.
    if manifest.share_id != expected_share_id {
        return Err(ManifestError::ShareIdMismatch);
    }
    // 5. Expiry.
    if manifest.expiry < now {
        return Err(ManifestError::Expired {
            expiry: manifest.expiry,
            now,
        });
    }
    Ok(manifest)
}

/// Verify a signed manifest, including the anti-rollback seqno check. On success
/// returns the decoded [`Manifest`]; the caller must still confirm the merkle
/// root against the file set via [`verify_root`].
pub fn verify(signed: &SignedManifest, ctx: &VerifyContext) -> Result<Manifest, ManifestError> {
    let manifest = verify_signed(signed, ctx.pinned_master, ctx.expected_share_id, ctx.now)?;
    // Anti-rollback: strictly increasing seqno.
    if manifest.seqno <= ctx.last_seqno {
        return Err(ManifestError::StaleSeqno {
            got: manifest.seqno,
            last: ctx.last_seqno,
        });
    }
    Ok(manifest)
}

/// Confirm the manifest's merkle root matches the actual file set the peer holds.
pub fn verify_root(manifest: &Manifest, entries: &[FileEntry]) -> Result<(), ManifestError> {
    if Manifest::compute_root(entries) == manifest.merkle_root {
        Ok(())
    } else {
        Err(ManifestError::RootMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn sample_entries() -> Vec<FileEntry> {
        vec![
            FileEntry {
                path: "a.txt".into(),
                hash: blake3::hash(b"a").as_bytes().to_vec(),
                size: 1,
                deleted: false,
            },
            FileEntry {
                path: "dir/b.bin".into(),
                hash: blake3::hash(b"bb").as_bytes().to_vec(),
                size: 2,
                deleted: false,
            },
        ]
    }

    fn build(key: &SigningKey, seqno: u64, expiry: i64, entries: &[FileEntry]) -> SignedManifest {
        let share_id = blake3::hash(&key.verifying_key().to_bytes())
            .as_bytes()
            .to_vec();
        let manifest = Manifest::new(share_id, seqno, expiry, entries.to_vec(), vec![]);
        sign(&manifest, key).unwrap()
    }

    #[test]
    fn root_is_order_independent() {
        let mut e = sample_entries();
        let r1 = Manifest::compute_root(&e);
        e.reverse();
        let r2 = Manifest::compute_root(&e);
        assert_eq!(r1, r2);
    }

    #[test]
    fn valid_manifest_verifies() {
        let key = SigningKey::generate(&mut OsRng);
        let entries = sample_entries();
        let signed = build(&key, 1, 9_999_999_999, &entries);
        let share_id = blake3::hash(&key.verifying_key().to_bytes())
            .as_bytes()
            .to_vec();
        let ctx = VerifyContext {
            pinned_master: &key.verifying_key(),
            expected_share_id: &share_id,
            last_seqno: 0,
            now: 1_000,
        };
        let m = verify(&signed, &ctx).unwrap();
        verify_root(&m, &entries).unwrap();
    }

    #[test]
    fn rejects_wrong_signer() {
        let key = SigningKey::generate(&mut OsRng);
        let attacker = SigningKey::generate(&mut OsRng);
        let entries = sample_entries();
        let signed = build(&key, 1, 9_999_999_999, &entries);
        let share_id = blake3::hash(&key.verifying_key().to_bytes())
            .as_bytes()
            .to_vec();
        let ctx = VerifyContext {
            pinned_master: &attacker.verifying_key(), // pinned to a different key
            expected_share_id: &share_id,
            last_seqno: 0,
            now: 1_000,
        };
        assert_eq!(verify(&signed, &ctx), Err(ManifestError::WrongSigner));
    }

    #[test]
    fn rejects_tampered_signature() {
        let key = SigningKey::generate(&mut OsRng);
        let entries = sample_entries();
        let mut signed = build(&key, 1, 9_999_999_999, &entries);
        signed.manifest_cbor[0] ^= 0xff; // tamper after signing
        let share_id = blake3::hash(&key.verifying_key().to_bytes())
            .as_bytes()
            .to_vec();
        let ctx = VerifyContext {
            pinned_master: &key.verifying_key(),
            expected_share_id: &share_id,
            last_seqno: 0,
            now: 1_000,
        };
        assert_eq!(verify(&signed, &ctx), Err(ManifestError::BadSignature));
    }

    #[test]
    fn rejects_stale_seqno() {
        let key = SigningKey::generate(&mut OsRng);
        let entries = sample_entries();
        let signed = build(&key, 5, 9_999_999_999, &entries);
        let share_id = blake3::hash(&key.verifying_key().to_bytes())
            .as_bytes()
            .to_vec();
        let ctx = VerifyContext {
            pinned_master: &key.verifying_key(),
            expected_share_id: &share_id,
            last_seqno: 5, // already saw seqno 5
            now: 1_000,
        };
        assert_eq!(
            verify(&signed, &ctx),
            Err(ManifestError::StaleSeqno { got: 5, last: 5 })
        );
    }

    #[test]
    fn rejects_expired() {
        let key = SigningKey::generate(&mut OsRng);
        let entries = sample_entries();
        let signed = build(&key, 1, 500, &entries);
        let share_id = blake3::hash(&key.verifying_key().to_bytes())
            .as_bytes()
            .to_vec();
        let ctx = VerifyContext {
            pinned_master: &key.verifying_key(),
            expected_share_id: &share_id,
            last_seqno: 0,
            now: 1_000, // past expiry 500
        };
        assert_eq!(
            verify(&signed, &ctx),
            Err(ManifestError::Expired {
                expiry: 500,
                now: 1_000
            })
        );
    }

    #[test]
    fn rejects_root_mismatch() {
        let key = SigningKey::generate(&mut OsRng);
        let entries = sample_entries();
        let signed = build(&key, 1, 9_999_999_999, &entries);
        let share_id = blake3::hash(&key.verifying_key().to_bytes())
            .as_bytes()
            .to_vec();
        let ctx = VerifyContext {
            pinned_master: &key.verifying_key(),
            expected_share_id: &share_id,
            last_seqno: 0,
            now: 1_000,
        };
        let m = verify(&signed, &ctx).unwrap();
        // A different file set than what was signed must fail the root check.
        let tampered = vec![FileEntry {
            path: "evil.txt".into(),
            hash: blake3::hash(b"evil").as_bytes().to_vec(),
            size: 4,
            deleted: false,
        }];
        assert_eq!(verify_root(&m, &tampered), Err(ManifestError::RootMismatch));
    }
}
