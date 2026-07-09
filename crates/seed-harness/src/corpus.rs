//! Deterministic file-corpus generation for sync tests and soaks.
//!
//! Everything derives from a `u64` seed via xorshift64*, so two runs with the
//! same spec produce byte-identical trees — a corpus can be regenerated for
//! comparison instead of stored. Content is non-compressible-ish (whitened RNG
//! words), file sizes are drawn per [`SizeBucket`], and large files are written
//! and hashed in streamed 4 MiB chunks so a 6 GB ISO never lives in memory.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::Context;

/// Streaming chunk size for generation, hashing, and verification.
const CHUNK: usize = 4 * 1024 * 1024;

/// xorshift64* — tiny, deterministic, good-enough whitening for test bytes.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Never zero (xorshift fixpoint).
        Rng(seed.wrapping_mul(2_685_821_657_736_338_717).max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(2_685_821_657_736_338_717)
    }

    /// Uniform-ish draw in `[lo, hi]`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }
}

/// `count` files with sizes drawn uniformly from `[min, max]` bytes.
#[derive(Clone, Copy, Debug)]
pub struct SizeBucket {
    pub count: usize,
    pub min: u64,
    pub max: u64,
    /// Short label folded into filenames (`f00042-small.bin`) so a listing shows
    /// the mix at a glance.
    pub label: &'static str,
}

/// A deterministic corpus: same spec + seed → byte-identical tree.
#[derive(Clone, Debug)]
pub struct CorpusSpec {
    pub seed: u64,
    pub buckets: Vec<SizeBucket>,
    /// Files are scattered into nested `dNN/` directories up to this depth.
    pub max_dir_depth: usize,
}

impl CorpusSpec {
    /// Scaled-down mix for repeatable automated runs: ~576 files, ~1.2 GB total,
    /// with the top bucket crossing the 4 MiB swarm threshold and the largest
    /// files big enough to exercise multi-round downloads without soak runtimes.
    pub fn scaled() -> Self {
        CorpusSpec {
            seed: 0x5EED_5EED,
            buckets: vec![
                SizeBucket {
                    count: 500,
                    min: 1 << 10,
                    max: 64 << 10,
                    label: "small",
                },
                SizeBucket {
                    count: 60,
                    min: 128 << 10,
                    max: 2 << 20,
                    label: "mid",
                },
                SizeBucket {
                    count: 12,
                    min: 4 << 20,
                    max: 16 << 20,
                    label: "swarm",
                },
                SizeBucket {
                    count: 4,
                    min: 48 << 20,
                    max: 96 << 20,
                    label: "big",
                },
            ],
            max_dir_depth: 4,
        }
    }

    /// The production-like workload (full-size soak only): thousands of mixed
    /// files plus 6 ISO-sized 3–6 GB files, ~30–35 GB per copy.
    pub fn full() -> Self {
        CorpusSpec {
            seed: 0x5EED_F011,
            buckets: vec![
                SizeBucket {
                    count: 3000,
                    min: 1 << 10,
                    max: 100 << 10,
                    label: "small",
                },
                SizeBucket {
                    count: 800,
                    min: 100 << 10,
                    max: 4 << 20,
                    label: "mid",
                },
                SizeBucket {
                    count: 200,
                    min: 4 << 20,
                    max: 100 << 20,
                    label: "large",
                },
                SizeBucket {
                    count: 6,
                    min: 3 << 30,
                    max: 6 << 30,
                    label: "iso",
                },
            ],
            max_dir_depth: 5,
        }
    }

    /// Mid-size mix for quick throughput readings (~6–8 GB per copy): a
    /// realistic small/mid spread plus a few genuinely large files, so a
    /// per-node MiB/s figure stabilizes in minutes — enough signal to rate a
    /// disk or an IO-shaping change without a multi-hour ISO-scale soak.
    pub fn midsize() -> Self {
        CorpusSpec {
            seed: 0x5EED_A11D,
            buckets: vec![
                SizeBucket {
                    count: 300,
                    min: 1 << 10,
                    max: 64 << 10,
                    label: "small",
                },
                SizeBucket {
                    count: 40,
                    min: 128 << 10,
                    max: 2 << 20,
                    label: "mid",
                },
                SizeBucket {
                    count: 10,
                    min: 4 << 20,
                    max: 16 << 20,
                    label: "swarm",
                },
                SizeBucket {
                    count: 5,
                    min: 512 << 20,
                    max: 1 << 30,
                    label: "big",
                },
            ],
            max_dir_depth: 4,
        }
    }

    /// Total bytes this spec will generate (upper bound: bucket maxima).
    pub fn max_bytes(&self) -> u64 {
        self.buckets.iter().map(|b| b.count as u64 * b.max).sum()
    }
}

/// Relative path → (size, blake3 hex). Small enough to hold for thousands of
/// files; content comparison streams against it rather than loading bytes.
pub type CorpusManifest = BTreeMap<String, (u64, String)>;

/// Deterministic relative path for file `idx` of `bucket`.
fn gen_path(rng: &mut Rng, spec: &CorpusSpec, label: &str, idx: usize) -> String {
    let depth = (rng.next_u64() as usize) % (spec.max_dir_depth + 1);
    let mut parts = Vec::with_capacity(depth + 1);
    for _ in 0..depth {
        parts.push(format!("d{:02}", rng.range(0, 19)));
    }
    parts.push(format!("f{idx:05}-{label}.bin"));
    parts.join("/")
}

/// Write one file of `size` deterministic bytes, returning its blake3 hex.
fn write_file(abs: &Path, size: u64, rng: &mut Rng) -> anyhow::Result<String> {
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(abs).with_context(|| format!("create {}", abs.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut remaining = size;
    let mut buf = vec![0u8; CHUNK.min(size.max(1) as usize)];
    while remaining > 0 {
        let n = (remaining as usize).min(buf.len());
        // Fill in u64 words: ~8x fewer RNG calls than per-byte.
        for w in buf[..n].chunks_mut(8) {
            let v = rng.next_u64().to_le_bytes();
            let l = w.len();
            w.copy_from_slice(&v[..l]);
        }
        f.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }
    f.flush()?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Generate the corpus under `root`. Returns the manifest of what was written.
pub fn generate(root: &Path, spec: &CorpusSpec) -> anyhow::Result<CorpusManifest> {
    let mut rng = Rng::new(spec.seed);
    let mut manifest = CorpusManifest::new();
    for bucket in &spec.buckets {
        for idx in 0..bucket.count {
            // Re-draw on the rare path collision so `count` files always exist.
            let mut rel = gen_path(&mut rng, spec, bucket.label, idx);
            while manifest.contains_key(&rel) {
                rel = gen_path(&mut rng, spec, bucket.label, idx);
            }
            let size = rng.range(bucket.min, bucket.max);
            let hex = write_file(&root.join(&rel), size, &mut rng)?;
            manifest.insert(rel, (size, hex));
        }
    }
    Ok(manifest)
}

/// What a [`mutate`] round did, for logging/assertions.
#[derive(Debug, Default)]
pub struct MutationSummary {
    pub rewritten: Vec<String>,
    pub deleted: Vec<String>,
    pub added: Vec<String>,
}

/// Deterministic churn: rewrite/delete a fraction of existing files and add a
/// few new ones, updating `manifest` to the new expected state. `round` salts
/// the RNG so successive rounds differ but replay identically.
pub fn mutate(
    root: &Path,
    manifest: &mut CorpusManifest,
    spec_seed: u64,
    round: u64,
    frac: f64,
) -> anyhow::Result<MutationSummary> {
    let mut rng = Rng::new(spec_seed ^ round.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let paths: Vec<String> = manifest.keys().cloned().collect();
    let touch = ((paths.len() as f64 * frac).ceil() as usize).max(1);
    let mut out = MutationSummary::default();
    for _ in 0..touch {
        let victim = paths[(rng.next_u64() as usize) % paths.len()].clone();
        if !manifest.contains_key(&victim) {
            continue; // already deleted this round
        }
        match rng.range(0, 3) {
            // 0-1: rewrite in place with fresh content (size re-drawn nearby).
            0 | 1 => {
                let size = rng.range(1 << 10, 256 << 10);
                let hex = write_file(&root.join(&victim), size, &mut rng)?;
                manifest.insert(victim.clone(), (size, hex));
                out.rewritten.push(victim);
            }
            // 2: delete.
            2 => {
                let _ = std::fs::remove_file(root.join(&victim));
                manifest.remove(&victim);
                out.deleted.push(victim);
            }
            // 3: add a sibling.
            _ => {
                let rel = format!("churn/r{round:03}-{:08x}.bin", rng.next_u64() as u32);
                let size = rng.range(1 << 10, 256 << 10);
                let hex = write_file(&root.join(&rel), size, &mut rng)?;
                manifest.insert(rel.clone(), (size, hex));
                out.added.push(rel);
            }
        }
    }
    Ok(out)
}

/// Blake3 of a file, streamed.
pub fn hash_file(abs: &Path) -> anyhow::Result<String> {
    let mut f = std::fs::File::open(abs).with_context(|| format!("open {}", abs.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Compare a tree against a manifest by streaming hashes (never loads a whole
/// file). Returns human-readable mismatch lines; empty means byte-identical.
pub fn verify(root: &Path, manifest: &CorpusManifest) -> anyhow::Result<Vec<String>> {
    let mut problems = Vec::new();
    let mut seen = 0usize;
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        match manifest.get(&rel) {
            None => problems.push(format!("extra file: {rel}")),
            Some((size, hex)) => {
                seen += 1;
                let actual = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if actual != *size {
                    problems.push(format!("size mismatch: {rel} ({actual} != {size})"));
                } else if &hash_file(entry.path())? != hex {
                    problems.push(format!("content mismatch: {rel}"));
                }
            }
        }
    }
    if seen != manifest.len() {
        for rel in manifest.keys() {
            if !root.join(rel).is_file() {
                problems.push(format!("missing file: {rel}"));
            }
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Determinism is the whole point: same spec → identical manifest; a
    /// regenerated tree verifies clean; a tampered file is caught.
    #[test]
    fn generate_is_deterministic_and_verify_catches_tampering() {
        let spec = CorpusSpec {
            seed: 7,
            buckets: vec![SizeBucket {
                count: 20,
                min: 100,
                max: 5000,
                label: "t",
            }],
            max_dir_depth: 2,
        };
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ma = generate(a.path(), &spec).unwrap();
        let mb = generate(b.path(), &spec).unwrap();
        assert_eq!(ma, mb, "same spec must produce identical manifests");
        assert_eq!(ma.len(), 20);
        assert!(verify(a.path(), &ma).unwrap().is_empty());

        // Tamper one byte → verify reports exactly that file.
        let (rel, (size, _)) = ma.iter().next_back().unwrap();
        let p = a.path().join(rel);
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[(*size as usize) / 2] ^= 0xff;
        std::fs::write(&p, bytes).unwrap();
        let problems = verify(a.path(), &ma).unwrap();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains(rel.as_str()));
    }

    /// Churn must keep the manifest in lockstep with the tree.
    #[test]
    fn mutate_keeps_manifest_consistent() {
        let spec = CorpusSpec {
            seed: 9,
            buckets: vec![SizeBucket {
                count: 30,
                min: 100,
                max: 2000,
                label: "t",
            }],
            max_dir_depth: 1,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = generate(dir.path(), &spec).unwrap();
        for round in 0..3 {
            let s = mutate(dir.path(), &mut manifest, spec.seed, round, 0.2).unwrap();
            assert!(!(s.rewritten.is_empty() && s.deleted.is_empty() && s.added.is_empty()));
            assert!(
                verify(dir.path(), &manifest).unwrap().is_empty(),
                "tree must match manifest after round {round}"
            );
        }
    }
}
