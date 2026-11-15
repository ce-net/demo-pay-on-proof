//! The requester's jobs, made from a seed.

use serde::{Deserialize, Serialize};

use crate::{hash_u64, Rng};

/// Size of the shared file the hash jobs read ranges of.
pub const CORPUS_LEN: usize = 1 << 20;
/// Hash jobs cover this many blocks of [`HASH_BLOCK`] bytes.
pub const HASH_BLOCKS: u64 = 16;
pub const HASH_BLOCK: u64 = 4096;
/// Numbers in one sort shard.
pub const SHARD_LEN: usize = 400;

/// What a job asks for. Serialized into the task's `need` text, so the worker reads the same JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Spec {
    /// SHA-256 of each `block`-byte block of `corpus.bin[off .. off + blocks * block]`.
    Hash { off: u64, blocks: u64, block: u64 },
    /// Return `values` in non-decreasing order.
    Sort { values: Vec<u32> },
    /// Return x with A x = b.
    Solve { a: Vec<Vec<i64>>, b: Vec<i64> },
}

impl Spec {
    pub fn kind(&self) -> &'static str {
        match self {
            Spec::Hash { .. } => "hash",
            Spec::Sort { .. } => "sort",
            Spec::Solve { .. } => "solve",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    pub title: String,
    pub spec: Spec,
    /// Credits the reward token carries if the result passes.
    pub credits: u64,
}

impl Job {
    /// The task text a worker receives: a short prefix, then the spec as JSON.
    pub fn need(&self) -> String {
        format!("pay-on-proof job {}: {}", self.id, serde_json::to_string(&self.spec).unwrap())
    }
}

/// The shared file hash jobs read from (served by the board at `/corpus`).
pub fn corpus(seed: u64) -> Vec<u8> {
    let mut r = Rng::new(hash_u64(&[b"corpus", &seed.to_le_bytes()]));
    let mut out = Vec::with_capacity(CORPUS_LEN);
    while out.len() < CORPUS_LEN {
        out.extend_from_slice(&r.next_u64().to_le_bytes());
    }
    out
}

/// `n` jobs: hash a file range, sort a shard, or solve a small linear system.
pub fn generate(seed: u64, n: usize) -> Vec<Job> {
    let mut r = Rng::new(hash_u64(&[b"jobs", &seed.to_le_bytes()]));
    (0..n)
        .map(|i| {
            let id = format!("j-{:02}", i + 1);
            match r.below(3) {
                0 => {
                    let span = HASH_BLOCKS * HASH_BLOCK;
                    let off = r.below(CORPUS_LEN as u64 - span + 1);
                    Job {
                        title: format!("hash corpus.bin bytes {}..{}", off, off + span),
                        spec: Spec::Hash { off, blocks: HASH_BLOCKS, block: HASH_BLOCK },
                        credits: 3,
                        id,
                    }
                }
                1 => {
                    let values = (0..SHARD_LEN).map(|_| r.below(1_000_000) as u32).collect();
                    Job {
                        title: format!("sort shard {} ({} numbers)", i + 1, SHARD_LEN),
                        spec: Spec::Sort { values },
                        credits: 2,
                        id,
                    }
                }
                _ => {
                    let n = 4 + r.below(5) as usize;
                    let mut a = vec![vec![0i64; n]; n];
                    for (row_i, row) in a.iter_mut().enumerate() {
                        let mut off_diag = 0;
                        for (j, v) in row.iter_mut().enumerate() {
                            if j != row_i {
                                *v = r.below(19) as i64 - 9;
                                off_diag += v.abs();
                            }
                        }
                        // strictly diagonally dominant, so the system has exactly one solution
                        row[row_i] = off_diag + 1 + r.below(5) as i64;
                    }
                    let b = (0..n).map(|_| r.below(201) as i64 - 100).collect();
                    Job {
                        title: format!("solve a {n}x{n} linear system"),
                        spec: Spec::Solve { a, b },
                        credits: 5,
                        id,
                    }
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_jobs() {
        let a = serde_json::to_string(&generate(7, 50)).unwrap();
        let b = serde_json::to_string(&generate(7, 50)).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, serde_json::to_string(&generate(8, 50)).unwrap());
    }

    #[test]
    fn all_three_kinds_appear() {
        let jobs = generate(7, 50);
        for k in ["hash", "sort", "solve"] {
            assert!(jobs.iter().any(|j| j.spec.kind() == k), "{k}");
        }
    }
}
