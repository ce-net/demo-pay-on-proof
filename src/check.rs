//! `job.check`: a cheap verifier per job kind.
//!
//! Each verifier costs much less than doing the job: the hash check re-hashes 3 of 16 blocks, the
//! sort check is one pass plus a polynomial fingerprint, the solve check is one matrix-vector
//! product. Verifiers are looked up by kind, so a new job kind is one more [`Verifier`].

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::jobs::Spec;
use crate::{sha256_hex, Rng};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub pass: bool,
    /// Which check ran.
    pub check: String,
    /// What it found, in a few words.
    pub detail: String,
}

fn pass(check: &str, detail: String) -> Verdict {
    Verdict { pass: true, check: check.into(), detail }
}
fn fail(check: &str, detail: String) -> Verdict {
    Verdict { pass: false, check: check.into(), detail }
}

/// One job kind's verifier. `salt` is secret to the board and fixed per job, so a worker cannot
/// know in advance which blocks are spot-checked or which point the fingerprint is taken at.
pub trait Verifier: Send + Sync {
    fn kind(&self) -> &'static str;
    fn check(&self, spec: &Spec, result: &str, salt: u64) -> Verdict;
}

/// The registry: kind -> verifier.
#[derive(Default)]
pub struct Checker {
    verifiers: BTreeMap<&'static str, Box<dyn Verifier>>,
}

impl Checker {
    /// The three built-in verifiers. `corpus` is the file hash jobs read from.
    pub fn standard(corpus: Arc<Vec<u8>>) -> Self {
        let mut c = Checker::default();
        c.register(Box::new(HashSpot { corpus, samples: 3 }));
        c.register(Box::new(SortPermutation));
        c.register(Box::new(SolveResidual));
        c
    }
    pub fn register(&mut self, v: Box<dyn Verifier>) {
        self.verifiers.insert(v.kind(), v);
    }
    pub fn kinds(&self) -> Vec<&'static str> {
        self.verifiers.keys().copied().collect()
    }
    pub fn check(&self, spec: &Spec, result: &str, salt: u64) -> Verdict {
        match self.verifiers.get(spec.kind()) {
            Some(v) => v.check(spec, result, salt),
            None => fail("none", format!("no verifier for kind {}", spec.kind())),
        }
    }
}

/// Hash spot check: the result must list one hash per block; a few blocks, picked by the salt,
/// are re-hashed and must match.
pub struct HashSpot {
    pub corpus: Arc<Vec<u8>>,
    pub samples: usize,
}

impl Verifier for HashSpot {
    fn kind(&self) -> &'static str {
        "hash"
    }
    fn check(&self, spec: &Spec, result: &str, salt: u64) -> Verdict {
        const C: &str = "hash spot check";
        let Spec::Hash { off, blocks, block } = spec else { return fail(C, "not a hash job".into()) };
        let Ok(list) = serde_json::from_str::<Vec<String>>(result) else {
            return fail(C, "result is not a list of hashes".into());
        };
        if list.len() as u64 != *blocks {
            return fail(C, format!("{} hashes for {} blocks", list.len(), blocks));
        }
        let mut r = Rng::new(salt);
        let mut picked: Vec<u64> = Vec::new();
        while picked.len() < self.samples.min(*blocks as usize) {
            let i = r.below(*blocks);
            if !picked.contains(&i) {
                picked.push(i);
            }
        }
        picked.sort();
        for &i in &picked {
            let start = (off + i * block) as usize;
            let Some(bytes) = self.corpus.get(start..start + *block as usize) else {
                return fail(C, "range outside corpus".into());
            };
            if list[i as usize] != sha256_hex(bytes) {
                return fail(C, format!("block {} of {} does not match", i + 1, blocks));
            }
        }
        let names: Vec<String> = picked.iter().map(|i| (i + 1).to_string()).collect();
        pass(C, format!("blocks {} of {} re-hashed, all match", names.join(", "), blocks))
    }
}

/// Sortedness and permutation check: non-decreasing, same length, and the same multiset, by
/// comparing prod (r - x) mod p for a secret r (a wrong multiset collides with chance <= n / p).
pub struct SortPermutation;

const P: u128 = (1 << 61) - 1;

fn fingerprint(xs: impl Iterator<Item = u64>, r: u128) -> u128 {
    xs.fold(1u128, |acc, x| acc * ((r + P - (x as u128 % P)) % P) % P)
}

impl Verifier for SortPermutation {
    fn kind(&self) -> &'static str {
        "sort"
    }
    fn check(&self, spec: &Spec, result: &str, salt: u64) -> Verdict {
        const C: &str = "sortedness + permutation";
        let Spec::Sort { values } = spec else { return fail(C, "not a sort job".into()) };
        let Ok(out) = serde_json::from_str::<Vec<u64>>(result) else {
            return fail(C, "result is not a list of numbers".into());
        };
        if out.len() != values.len() {
            return fail(C, format!("{} numbers back for {}", out.len(), values.len()));
        }
        if let Some(i) = out.windows(2).position(|w| w[0] > w[1]) {
            return fail(C, format!("out of order at position {}", i + 2));
        }
        let r = (salt as u128) % (P - 1) + 1;
        if fingerprint(out.iter().copied(), r) != fingerprint(values.iter().map(|&v| v as u64), r) {
            return fail(C, "sorted, but not a permutation of the input".into());
        }
        pass(C, format!("{} numbers in order, same multiset", out.len()))
    }
}

/// Residual check: max |A x - b| must be tiny compared with b.
pub struct SolveResidual;

impl Verifier for SolveResidual {
    fn kind(&self) -> &'static str {
        "solve"
    }
    fn check(&self, spec: &Spec, result: &str, _salt: u64) -> Verdict {
        const C: &str = "residual";
        let Spec::Solve { a, b } = spec else { return fail(C, "not a solve job".into()) };
        let Ok(x) = serde_json::from_str::<Vec<Value>>(result) else {
            return fail(C, "result is not a list of numbers".into());
        };
        let x: Option<Vec<f64>> = x.iter().map(Value::as_f64).collect();
        let Some(x) = x.filter(|x| x.len() == b.len() && x.iter().all(|v| v.is_finite())) else {
            return fail(C, format!("need {} finite numbers", b.len()));
        };
        let mut worst = 0f64;
        for (row, &bi) in a.iter().zip(b) {
            let mut s = 0f64;
            for (&aij, &xj) in row.iter().zip(&x) {
                s += aij as f64 * xj;
            }
            worst = worst.max((s - bi as f64).abs());
        }
        let bmax = b.iter().map(|v| v.abs()).max().unwrap_or(0) as f64;
        let limit = 1e-9 * (1.0 + bmax);
        if worst <= limit {
            pass(C, format!("max |Ax - b| = {worst:.1e} (limit {limit:.1e})"))
        } else {
            fail(C, format!("max |Ax - b| = {worst:.1e}, over the limit {limit:.1e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{corpus, HASH_BLOCK};

    fn checker() -> (Checker, Arc<Vec<u8>>) {
        let c = Arc::new(corpus(7));
        (Checker::standard(c.clone()), c)
    }

    #[test]
    fn hash_honest_passes_shifted_fails() {
        let (ch, c) = checker();
        let spec = Spec::Hash { off: 1000, blocks: 16, block: HASH_BLOCK };
        let mk = |off: usize| -> String {
            let v: Vec<String> =
                (0..16).map(|i| sha256_hex(&c[off + i * 4096..off + (i + 1) * 4096])).collect();
            serde_json::to_string(&v).unwrap()
        };
        assert!(ch.check(&spec, &mk(1000), 42).pass);
        assert!(!ch.check(&spec, &mk(1001), 42).pass);
        assert!(!ch.check(&spec, "[\"00\"]", 42).pass);
    }

    #[test]
    fn sort_catches_disorder_and_swapped_values() {
        let (ch, _) = checker();
        let spec = Spec::Sort { values: vec![5, 3, 9, 3, 1] };
        assert!(ch.check(&spec, "[1,3,3,5,9]", 9).pass);
        assert!(!ch.check(&spec, "[1,3,5,3,9]", 9).pass);
        assert!(!ch.check(&spec, "[1,3,3,3,9]", 9).pass, "duplicated value");
        assert!(!ch.check(&spec, "[1,3,5,9]", 9).pass);
    }

    #[test]
    fn solve_residual() {
        let (ch, _) = checker();
        let spec = Spec::Solve { a: vec![vec![2, 1], vec![1, 3]], b: vec![3, 5] };
        // x = (0.8, 1.4)
        assert!(ch.check(&spec, "[0.8, 1.4]", 0).pass);
        assert!(!ch.check(&spec, "[0.801, 1.4]", 0).pass);
        assert!(!ch.check(&spec, "[0.8]", 0).pass);
        assert!(!ch.check(&spec, "\"x\"", 0).pass);
    }

    #[test]
    fn registry_lists_kinds() {
        let (ch, _) = checker();
        assert_eq!(ch.kinds(), vec!["hash", "solve", "sort"]);
    }
}
