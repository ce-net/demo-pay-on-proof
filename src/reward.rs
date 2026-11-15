//! `reward.issue`: a signed, single-use reward token for a result that passed its check.
//!
//! A token is a one-link ce-net capability chain ([`ce_net_proto`]), so anyone with the board's
//! public reward key can verify it offline with the same code ce-net uses for every capability:
//!
//! - issuer: the board's reward key; audience: the payee (see [`payee_id`]);
//! - abilities: `reward.redeem` and `job:<id>` (which job it pays for);
//! - caveats: `max_credits` = what the job pays;
//! - nonce: unique per job. Redeeming records (issuer, nonce) as spent, and a spent token fails
//!   verification through the chain's revocation check, so it pays once.

use std::collections::BTreeSet;

use ce_net_proto::{
    decode_chain, encode_chain, verify_chain, Caveats, Identity, NodeId, Resource, SignedCapability,
};
use serde::Serialize;

use crate::check::Verdict;
use crate::{hash_parts, hash_u64};

pub const ABILITY: &str = "reward.redeem";

/// The starter worker has no Ed25519 key of its own yet (ce-net gives it a `cen_` key), so the
/// token's audience is a stable 32-byte id derived from its agent id.
pub fn payee_id(agent: &str) -> NodeId {
    hash_parts(&[b"ce.pay-on-proof/payee@1", agent.as_bytes()])
}

pub struct Issuer {
    key: Identity,
    seed: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reward {
    pub credits: u64,
    pub nonce: u64,
    /// Hex of the encoded chain.
    pub token: String,
}

impl Issuer {
    /// A demo key derived from the seed, so a replay signs the same bytes. A real board loads its
    /// key from a file instead.
    pub fn demo(seed: u64) -> Self {
        let secret = hash_parts(&[b"pay-on-proof demo reward key", &seed.to_le_bytes()]);
        Issuer { key: Identity::from_secret_bytes(&secret), seed }
    }
    pub fn public_hex(&self) -> String {
        self.key.node_id_hex()
    }
    pub fn public(&self) -> NodeId {
        self.key.node_id()
    }

    /// Sign a reward for `job` done by `agent`, only if `verdict` passed.
    pub fn issue(&self, verdict: &Verdict, job: &str, agent: &str, credits: u64) -> Result<Reward, String> {
        if !verdict.pass {
            return Err(format!("{job}: the result did not pass ({}), no reward", verdict.detail));
        }
        let nonce = hash_u64(&[b"reward nonce", &self.seed.to_le_bytes(), job.as_bytes()]);
        let caveats = Caveats { max_credits: Some(credits), ..Caveats::default() };
        let cap = SignedCapability::issue(
            &self.key,
            payee_id(agent),
            vec![ABILITY.into(), format!("job:{job}")],
            Resource::Any,
            caveats,
            nonce,
            None,
        );
        Ok(Reward { credits, nonce, token: encode_chain(&[cap]) })
    }
}

/// What a valid token says.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Checked {
    pub job: String,
    pub payee: String,
    pub credits: u64,
    pub nonce: u64,
}

/// Verify `token` against the board's public key; `spent` holds redeemed nonces.
pub fn verify(token: &str, root: &NodeId, spent: &BTreeSet<u64>) -> Result<Checked, String> {
    let chain = decode_chain(token).map_err(|e| e.to_string())?;
    if chain.len() != 1 {
        return Err("a reward token is exactly one link".into());
    }
    let issuer = *root;
    let v = verify_chain(&chain, root, 0, &|who, nonce| who == &issuer && spent.contains(&nonce))?;
    if !v.abilities.iter().any(|a| a == ABILITY) {
        return Err("not a reward token".into());
    }
    let job = v
        .abilities
        .iter()
        .find_map(|a| a.strip_prefix("job:"))
        .ok_or("token names no job")?
        .to_string();
    let credits = v.caveats.max_credits.ok_or("token carries no credits")?;
    Ok(Checked { job, payee: hex::encode(v.holder), credits, nonce: chain[0].cap.nonce })
}

/// Redeem once: verify, then mark spent. A second redeem of the same token is refused.
pub fn redeem(token: &str, root: &NodeId, spent: &mut BTreeSet<u64>) -> Result<Checked, String> {
    let c = verify(token, root, spent).map_err(|e| {
        if e.contains("revoked") { "already redeemed".to_string() } else { e }
    })?;
    spent.insert(c.nonce);
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> Verdict {
        Verdict { pass: true, check: "t".into(), detail: "ok".into() }
    }

    #[test]
    fn issues_only_on_pass() {
        let i = Issuer::demo(7);
        let bad = Verdict { pass: false, check: "t".into(), detail: "wrong".into() };
        assert!(i.issue(&bad, "j-01", "a-1", 3).is_err());
        assert!(i.issue(&ok(), "j-01", "a-1", 3).is_ok());
    }

    #[test]
    fn verifies_and_pays_once() {
        let i = Issuer::demo(7);
        let r = i.issue(&ok(), "j-04", "a-1", 5).unwrap();
        let mut spent = BTreeSet::new();
        let c = redeem(&r.token, &i.public(), &mut spent).unwrap();
        assert_eq!((c.job.as_str(), c.credits), ("j-04", 5));
        assert_eq!(c.payee, hex::encode(payee_id("a-1")));
        assert_eq!(redeem(&r.token, &i.public(), &mut spent).unwrap_err(), "already redeemed");
    }

    #[test]
    fn deterministic_and_tamper_evident() {
        let a = Issuer::demo(7).issue(&ok(), "j-04", "a-1", 5).unwrap();
        let b = Issuer::demo(7).issue(&ok(), "j-04", "a-1", 5).unwrap();
        assert_eq!(a, b);
        let other = Issuer::demo(8);
        assert!(verify(&a.token, &other.public(), &BTreeSet::new()).is_err(), "wrong key");
        // flip one byte of the signed body
        let mut raw = hex::decode(&a.token).unwrap();
        let n = raw.len();
        raw[n - 80] ^= 1;
        assert!(verify(&hex::encode(raw), &Issuer::demo(7).public(), &BTreeSet::new()).is_err());
    }
}
