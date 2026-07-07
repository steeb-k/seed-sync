//! SEED Sync core sync engine.
//!
//! This crate owns everything that is *not* GUI or IPC transport: the iroh
//! endpoint and blob/doc/gossip stores, the multi-master sync engine, share
//! identity/keys, the filesystem scan + mirror loop, and local persistence.
//!
//! Trust model: the shared folder lives in one iroh-docs replica whose namespace
//! key is the share's master secret. Every doc entry is signed by that namespace
//! key, so only holders of the master key can write — viewers hold a read-only
//! capability and physically cannot. Multiple masters write into the same replica
//! (different authors) and iroh-docs merges their entries last-writer-wins. The
//! [`engine`] reconciles every node's folder to the merged replica. ([`manifest`]
//! still provides the shared [`scan`](crate::scan) file-entry type; its earlier
//! role as a separate signed trust root has been superseded by the replica's own
//! per-entry signatures.)

pub mod db;
pub mod engine;
pub mod health;
pub mod identity;
pub mod manifest;
pub mod node;
pub mod presence;
pub mod scan;
pub mod secrets;

pub use engine::Engine;
pub use identity::{Role, ShareKey};
pub use manifest::{Manifest, SignedManifest, VerifyContext};
pub use node::IrohNode;

/// Crate-wide result alias.
pub type Result<T> = anyhow::Result<T>;
