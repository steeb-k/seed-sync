//! Seed Sync core sync engine.
//!
//! This crate owns everything that is *not* GUI or IPC transport: the iroh
//! endpoint and blob/doc/gossip stores, the signed-manifest trust model, share
//! identity/keys, the filesystem scan + mirror loop, and local persistence.
//!
//! Build order (see plan): the pure-crypto trust root ([`manifest`]) and key
//! encoding ([`identity`]) come first and are fully testable without iroh; the
//! networked [`engine`] is layered on top.

pub mod identity;
pub mod manifest;
pub mod scan;

pub use identity::{Role, ShareKey};
pub use manifest::{Manifest, SignedManifest, VerifyContext};

/// Crate-wide result alias.
pub type Result<T> = anyhow::Result<T>;
