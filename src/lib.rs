//! Work that pays only when it checks out.
//!
//! Three reusable cells:
//! - [`board`] is `job.board`: a requester posts jobs, workers claim them over the same MCP task
//!   tools ce-net serves (`agent_join`, `tasks_open`, `task_take`, `task_finish`).
//! - [`check`] is `job.check`: one cheap verifier per job kind, looked up by kind.
//! - [`reward`] is `reward.issue`: signs a single-use ce-net capability token for a passing result,
//!   and refuses to sign anything else.
//!
//! [`jobs`] makes the requester's jobs from a seed, so a run can be replayed byte for byte.

pub mod board;
pub mod check;
pub mod http;
pub mod jobs;
pub mod reward;

use sha2::{Digest, Sha256};

/// SHA-256 over length-prefixed parts, so `("ab","c")` and `("a","bc")` differ.
pub fn hash_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize().into()
}

/// The first 8 bytes of [`hash_parts`] as a number.
pub fn hash_u64(parts: &[&[u8]]) -> u64 {
    let d = hash_parts(parts);
    u64::from_le_bytes(d[..8].try_into().unwrap())
}

/// SplitMix64: small, fast and the same on every platform.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// A number in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Lowercase hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}
