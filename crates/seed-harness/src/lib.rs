//! Test/soak harness for SEED Sync. Dev-only workspace member — a dependency of
//! integration tests and the `seed-soak` bin, never of shipped crates.
//!
//! - [`corpus`] — deterministic, seeded file-corpus generation and verification,
//!   sized from "hundreds of small files" up to the full production-like workload
//!   (thousands of mixed files + multi-GB ISOs), streamed so multi-GB files never
//!   live in memory.
//! - [`proc`] — spawn and drive real `seed-daemon` processes over IPC (extracted
//!   from the `loopback_ipc` test so soaks and daemon tests share one driver).

pub mod corpus;
pub mod proc;
