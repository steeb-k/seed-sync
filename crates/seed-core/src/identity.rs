//! Share keys and identities.
//!
//! Two key spaces are kept deliberately separate:
//!
//! * **Device identity** — the persistent iroh node key, generated once per
//!   install and reused across every share. (Managed by the engine, not here.)
//! * **Share capability keys** — per-share, copy/pasteable strings:
//!     * a **master key** carries the Ed25519 *signing seed* (write access), and
//!     * a **viewer key** carries only the *public verifying key* (read-only).
//!
//! `share_id = BLAKE3(master_pub)`, so any holder of either key can confirm the
//! id matches the pinned public key, defeating a swapped-pubkey attack. The
//! write-prevention guarantee is physical: a viewer key contains no seed, so a
//! viewer cannot produce a signature any peer will accept.

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

const SHARE_KEY_VERSION: u8 = 1;
const HRP_MASTER: bech32::Hrp = bech32::Hrp::parse_unchecked("seedm");
const HRP_VIEWER: bech32::Hrp = bech32::Hrp::parse_unchecked("seedv");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Master,
    Viewer,
}

/// The decoded contents of a share key string.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShareKeyPayload {
    version: u8,
    /// 32 bytes.
    #[serde(with = "serde_bytes")]
    master_pub: Vec<u8>,
    /// Present only for master keys (32-byte signing seed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(with = "serde_bytes_opt")]
    master_seed: Option<Vec<u8>>,
}

/// A parsed share key, exposing role-appropriate cryptographic material.
#[derive(Debug, Clone)]
pub struct ShareKey {
    pub role: Role,
    pub master_pub: VerifyingKey,
    /// `Some` only for master keys.
    seed: Option<[u8; 32]>,
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("bech32 decode: {0}")]
    Bech32(String),
    #[error("unexpected key prefix '{0}'")]
    BadPrefix(String),
    #[error("unsupported key version {0}")]
    BadVersion(u8),
    #[error("malformed key payload: {0}")]
    Payload(String),
    #[error("bad key length")]
    BadLength,
}

impl ShareKey {
    /// Generate a brand-new master key for a fresh share.
    pub fn generate_master() -> Self {
        use rand::rngs::OsRng;
        let signing = SigningKey::generate(&mut OsRng);
        Self {
            role: Role::Master,
            master_pub: signing.verifying_key(),
            seed: Some(signing.to_bytes()),
        }
    }

    /// The 32-byte share id = BLAKE3(master_pub).
    pub fn share_id(&self) -> [u8; 32] {
        *blake3::hash(&self.master_pub.to_bytes()).as_bytes()
    }

    /// Hex form of the share id, used as the IPC `ShareId`.
    pub fn share_id_hex(&self) -> String {
        data_encoding::HEXLOWER.encode(&self.share_id())
    }

    /// The signing key, available only for master keys.
    pub fn signing_key(&self) -> Option<SigningKey> {
        self.seed.map(|seed| SigningKey::from_bytes(&seed))
    }

    /// Derive the viewer-equivalent of this key (drops the seed). For a viewer
    /// key this is a clone.
    pub fn as_viewer(&self) -> ShareKey {
        ShareKey {
            role: Role::Viewer,
            master_pub: self.master_pub,
            seed: None,
        }
    }

    /// Encode this key as a copy/pasteable bech32m string.
    pub fn encode(&self) -> String {
        let payload = ShareKeyPayload {
            version: SHARE_KEY_VERSION,
            master_pub: self.master_pub.to_bytes().to_vec(),
            master_seed: match self.role {
                Role::Master => self.seed.map(|s| s.to_vec()),
                Role::Viewer => None,
            },
        };
        let mut cbor = Vec::new();
        ciborium::into_writer(&payload, &mut cbor).expect("cbor encode share key");
        let hrp = match self.role {
            Role::Master => HRP_MASTER,
            Role::Viewer => HRP_VIEWER,
        };
        bech32::encode::<bech32::Bech32m>(hrp, &cbor).expect("bech32 encode")
    }

    /// Encode the viewer key string for this share (always seedless).
    pub fn encode_viewer(&self) -> String {
        self.as_viewer().encode()
    }

    /// Parse a share key string. The HRP determines the role; a viewer string
    /// can never yield a signing key even if bytes were appended.
    pub fn decode(s: &str) -> Result<ShareKey, KeyError> {
        let (hrp, data) = bech32::decode(s.trim()).map_err(|e| KeyError::Bech32(e.to_string()))?;
        let role = if hrp == HRP_MASTER {
            Role::Master
        } else if hrp == HRP_VIEWER {
            Role::Viewer
        } else {
            return Err(KeyError::BadPrefix(hrp.to_string()));
        };
        let payload: ShareKeyPayload =
            ciborium::from_reader(&data[..]).map_err(|e| KeyError::Payload(e.to_string()))?;
        if payload.version != SHARE_KEY_VERSION {
            return Err(KeyError::BadVersion(payload.version));
        }
        let pub_bytes: [u8; 32] = payload
            .master_pub
            .as_slice()
            .try_into()
            .map_err(|_| KeyError::BadLength)?;
        let master_pub = VerifyingKey::from_bytes(&pub_bytes).map_err(|_| KeyError::BadLength)?;
        // Only honor a seed when the HRP says master; defensively ignore it otherwise.
        let seed = match role {
            Role::Master => match payload.master_seed {
                Some(s) => Some(s.as_slice().try_into().map_err(|_| KeyError::BadLength)?),
                None => return Err(KeyError::Payload("master key missing seed".into())),
            },
            Role::Viewer => None,
        };
        Ok(ShareKey {
            role,
            master_pub,
            seed,
        })
    }
}

/// serde helper for `Option<Vec<u8>>` as bytes.
mod serde_bytes_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(bytes) => serde_bytes::serialize(bytes, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let opt: Option<serde_bytes::ByteBuf> = Option::deserialize(d)?;
        Ok(opt.map(|b| b.into_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_roundtrip_preserves_signing() {
        let m = ShareKey::generate_master();
        let s = m.encode();
        assert!(s.starts_with("seedm1"));
        let back = ShareKey::decode(&s).unwrap();
        assert_eq!(back.role, Role::Master);
        assert!(back.signing_key().is_some());
        assert_eq!(back.share_id(), m.share_id());
    }

    #[test]
    fn viewer_has_no_seed() {
        let m = ShareKey::generate_master();
        let vs = m.encode_viewer();
        assert!(vs.starts_with("seedv1"));
        let v = ShareKey::decode(&vs).unwrap();
        assert_eq!(v.role, Role::Viewer);
        assert!(v.signing_key().is_none());
        // Same share, same pinned pubkey.
        assert_eq!(v.share_id(), m.share_id());
        assert_eq!(v.master_pub, m.master_pub);
    }

    #[test]
    fn share_id_binds_to_pubkey() {
        let m = ShareKey::generate_master();
        assert_eq!(
            m.share_id().to_vec(),
            blake3::hash(&m.master_pub.to_bytes()).as_bytes().to_vec()
        );
    }

    #[test]
    fn rejects_corrupt_checksum() {
        let m = ShareKey::generate_master();
        let mut s = m.encode();
        // Flip a char in the data section to break the bech32 checksum.
        let bytes = unsafe { s.as_bytes_mut() };
        let i = bytes.len() - 2;
        bytes[i] = if bytes[i] == b'q' { b'p' } else { b'q' };
        assert!(ShareKey::decode(&s).is_err());
    }
}
