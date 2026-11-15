//! `job.board`: the requester posts jobs; workers claim and finish them over MCP.
//!
//! It speaks the same tools a worker uses on ce-net (`agent_join`, `agent_me`, `tasks_open`,
//! `task_take`, `task_finish`), so the public task-worker-loop worker runs against it unchanged.
//! Every finished result goes through `job.check`; a pass gets a token from `reward.issue`.
//!
//! So that a run replays byte for byte, the requester splits the jobs between the workers by a
//! seeded draw (worker 1 = the first to join). `tasks_open` waits until every expected worker has
//! joined, then shows each worker its own open jobs.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::check::{Checker, Verdict};
use crate::jobs::{corpus, generate, Job};
use crate::reward::{Issuer, Reward};
use crate::{hash_parts, hash_u64, sha256_hex, Rng};

pub const SKILL: &str = "pay-on-proof";

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Open,
    Taken,
    Paid,
    Rejected,
}

struct Slot {
    job: Job,
    lane: usize,
    state: State,
    worker: Option<usize>,
    result_sha256: Option<String>,
    verdict: Option<Verdict>,
    reward: Option<Reward>,
}

struct Worker {
    name: String,
    agent: String,
    passed: u64,
    rejected: u64,
    credits: u64,
    /// One letter per checked result in order: P passed, R rejected.
    history: String,
}

pub struct Board {
    seed: u64,
    expect: usize,
    slots: Vec<Slot>,
    workers: Vec<Worker>,
    keys: BTreeMap<String, usize>,
    checker: Checker,
    issuer: Issuer,
    corpus: Arc<Vec<u8>>,
    started: Instant,
    finished_at: Option<f64>,
    pub report_path: Option<PathBuf>,
    pub quiet: bool,
}

#[derive(Serialize)]
struct JobRow<'a> {
    id: &'a str,
    kind: &'a str,
    title: &'a str,
    worker: Option<&'a str>,
    state: State,
    result_sha256: Option<&'a str>,
    check: Option<&'a str>,
    detail: Option<&'a str>,
    reward: Option<&'a Reward>,
}

#[derive(Serialize)]
struct WorkerRow<'a> {
    name: &'a str,
    agent: &'a str,
    checked: u64,
    passed: u64,
    rejected: u64,
    acceptance: String,
    credits: u64,
    history: &'a str,
}

#[derive(Serialize)]
struct Totals {
    jobs: usize,
    checked: usize,
    paid: usize,
    rejected: usize,
    credits_issued: u64,
}

#[derive(Serialize)]
struct Report<'a> {
    demo: &'static str,
    seed: u64,
    reward_key: String,
    verifiers: Vec<&'static str>,
    totals: Totals,
    workers: Vec<WorkerRow<'a>>,
    jobs: Vec<JobRow<'a>>,
}

impl Board {
    pub fn new(seed: u64, n_jobs: usize, expect: usize) -> Self {
        let corpus = Arc::new(corpus(seed));
        let mut lanes = Rng::new(hash_u64(&[b"lanes", &seed.to_le_bytes()]));
        let slots = generate(seed, n_jobs)
            .into_iter()
            .map(|job| Slot {
                job,
                lane: lanes.below(expect as u64) as usize,
                state: State::Open,
                worker: None,
                result_sha256: None,
                verdict: None,
                reward: None,
            })
            .collect();
        Board {
            seed,
            expect,
            slots,
            workers: Vec::new(),
            keys: BTreeMap::new(),
            checker: Checker::standard(corpus.clone()),
            issuer: Issuer::demo(seed),
            corpus,
            started: Instant::now(),
            finished_at: None,
            report_path: None,
            quiet: false,
        }
    }

    pub fn corpus(&self) -> &[u8] {
        &self.corpus
    }

    /// (job id, kind, worker lane) for every job, in order.
    pub fn plan(&self) -> Vec<(String, &'static str, usize)> {
        self.slots.iter().map(|s| (s.job.id.clone(), s.job.spec.kind(), s.lane)).collect()
    }

    pub fn done(&self) -> bool {
        self.slots.iter().all(|s| matches!(s.state, State::Paid | State::Rejected))
    }

    fn key_hash(key: &str) -> String {
        sha256_hex(key.as_bytes())
    }

    pub fn join(&mut self, name: &str, about: &str) -> Result<Value, String> {
        if self.workers.len() >= self.expect {
            return Err(format!("this board takes {} workers and has them all", self.expect));
        }
        let n = self.workers.len();
        let agent = format!(
            "a-{}",
            &hex::encode(hash_parts(&[b"agent", &self.seed.to_le_bytes(), &(n as u64).to_le_bytes()]))[..12]
        );
        let now = self.started.elapsed().as_nanos() as u64 ^ std::process::id() as u64;
        let key = format!("cen_{}", hex::encode(hash_parts(&[b"key", agent.as_bytes(), &now.to_le_bytes(), &rand_bytes()])));
        self.keys.insert(Self::key_hash(&key), n);
        let wname = format!("w{}", n + 1);
        self.workers.push(Worker { name: wname.clone(), agent: agent.clone(), passed: 0, rejected: 0, credits: 0, history: String::new() });
        self.log(format!("{wname} joined as {agent} ({name})"));
        Ok(json!({"agent": {"id": agent, "name": name, "about": about, "board_name": wname}, "key": key}))
    }

    pub fn worker_of(&self, key: &str) -> Result<usize, String> {
        self.keys.get(&Self::key_hash(key)).copied().ok_or_else(|| "sign in first: call agent_join and pass its key".into())
    }

    pub fn ready(&self) -> bool {
        self.workers.len() >= self.expect
    }

    pub fn open_for(&self, w: usize) -> Value {
        let tasks: Vec<Value> = self
            .slots
            .iter()
            .filter(|s| s.lane == w && s.state == State::Open)
            .map(|s| json!({"id": s.job.id, "title": format!("{SKILL}: {}", s.job.title), "need": s.job.need(), "state": "open", "skills": [SKILL], "credits": s.job.credits}))
            .collect();
        json!({"tasks": tasks})
    }

    fn slot(&mut self, id: &str) -> Result<&mut Slot, String> {
        self.slots.iter_mut().find(|s| s.job.id == id).ok_or_else(|| format!("no job {id}"))
    }

    pub fn take(&mut self, w: usize, id: &str) -> Result<Value, String> {
        let s = self.slot(id)?;
        if s.lane != w {
            return Err(format!("{id} is offered to another worker"));
        }
        if s.state != State::Open {
            return Err(format!("{id} is already taken"));
        }
        s.state = State::Taken;
        s.worker = Some(w);
        Ok(json!({"id": id, "title": s.job.title, "need": s.job.need(), "state": "taken"}))
    }

    pub fn finish(&mut self, w: usize, id: &str, result: &str) -> Result<Value, String> {
        let salt = hash_u64(&[b"check salt", &self.seed.to_le_bytes(), id.as_bytes()]);
        let agent = self.workers[w].agent.clone();
        let wname = self.workers[w].name.clone();
        let s = self.slots.iter().position(|s| s.job.id == id).ok_or_else(|| format!("no job {id}"))?;
        if self.slots[s].state != State::Taken || self.slots[s].worker != Some(w) {
            return Err(format!("{id} is not taken by you"));
        }
        let verdict = self.checker.check(&self.slots[s].job.spec, result, salt);
        let reward = self.issuer.issue(&verdict, id, &agent, self.slots[s].job.credits).ok();
        let slot = &mut self.slots[s];
        slot.result_sha256 = Some(sha256_hex(result.as_bytes()));
        slot.state = if reward.is_some() { State::Paid } else { State::Rejected };
        let worker = &mut self.workers[w];
        if let Some(r) = &reward {
            worker.passed += 1;
            worker.credits += r.credits;
            worker.history.push('P');
        } else {
            worker.rejected += 1;
            worker.history.push('R');
        }
        let rate = acceptance(worker.passed, worker.rejected);
        let line = format!(
            "{id} {:<5} {wname}  {:<8} {}   {wname} acceptance {rate}",
            slot.job.spec.kind(),
            if reward.is_some() { "PAID" } else { "REJECTED" },
            verdict.detail
        );
        let out = json!({"id": id, "state": if reward.is_some() {"accepted"} else {"rejected"}, "verdict": verdict, "reward": reward});
        slot.verdict = Some(verdict);
        slot.reward = reward;
        self.log(line);
        if self.done() && self.finished_at.is_none() {
            self.finished_at = Some(self.started.elapsed().as_secs_f64());
            if let Some(p) = &self.report_path {
                if let Some(dir) = p.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                std::fs::write(p, self.report_text()).map_err(|e| e.to_string())?;
                self.log(format!("all {} jobs checked; report written to {}", self.slots.len(), p.display()));
            }
            self.summary();
        }
        Ok(out)
    }

    fn log(&self, line: String) {
        if !self.quiet {
            println!("{line}");
        }
    }

    fn summary(&self) {
        let r = self.report_value();
        let t = &r["totals"];
        self.log(format!("paid {} of {} results, rejected {}, {} credits issued", t["paid"], t["jobs"], t["rejected"], t["credits_issued"]));
        for w in r["workers"].as_array().unwrap() {
            self.log(format!("  {} {}: {} passed, {} rejected, acceptance {}", w["name"].as_str().unwrap(), w["agent"].as_str().unwrap(), w["passed"], w["rejected"], w["acceptance"].as_str().unwrap()));
        }
    }

    fn report(&self) -> Report<'_> {
        let jobs: Vec<JobRow> = self
            .slots
            .iter()
            .map(|s| JobRow {
                id: &s.job.id,
                kind: s.job.spec.kind(),
                title: &s.job.title,
                worker: s.worker.map(|w| self.workers[w].name.as_str()),
                state: s.state,
                result_sha256: s.result_sha256.as_deref(),
                check: s.verdict.as_ref().map(|v| v.check.as_str()),
                detail: s.verdict.as_ref().map(|v| v.detail.as_str()),
                reward: s.reward.as_ref(),
            })
            .collect();
        let workers = self
            .workers
            .iter()
            .map(|w| WorkerRow {
                name: &w.name,
                agent: &w.agent,
                checked: w.passed + w.rejected,
                passed: w.passed,
                rejected: w.rejected,
                acceptance: acceptance(w.passed, w.rejected),
                credits: w.credits,
                history: &w.history,
            })
            .collect();
        let paid = self.slots.iter().filter(|s| s.state == State::Paid).count();
        let rejected = self.slots.iter().filter(|s| s.state == State::Rejected).count();
        Report {
            demo: "pay-on-proof",
            seed: self.seed,
            reward_key: self.issuer.public_hex(),
            verifiers: self.checker.kinds(),
            totals: Totals {
                jobs: self.slots.len(),
                checked: paid + rejected,
                paid,
                rejected,
                credits_issued: self.slots.iter().filter_map(|s| s.reward.as_ref()).map(|r| r.credits).sum(),
            },
            workers,
            jobs,
        }
    }

    pub fn report_value(&self) -> Value {
        serde_json::to_value(self.report()).unwrap()
    }

    /// The committed report: deterministic for a seed (no times, no keys, only results).
    pub fn report_text(&self) -> String {
        serde_json::to_string_pretty(&self.report()).unwrap() + "\n"
    }

    /// What the live page polls: the report so far plus progress.
    pub fn state(&self) -> Value {
        let mut v = self.report_value();
        v["live"] = json!(true);
        v["expect_workers"] = json!(self.expect);
        v["done"] = json!(self.done());
        v["elapsed_s"] = json!(self.finished_at.unwrap_or_else(|| self.started.elapsed().as_secs_f64()));
        v
    }
}

pub fn acceptance(passed: u64, rejected: u64) -> String {
    let n = passed + rejected;
    if n == 0 {
        "-".into()
    } else {
        format!("{:.1}%", 100.0 * passed as f64 / n as f64)
    }
}

fn rand_bytes() -> [u8; 16] {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    }
    b
}

/// The board plus a condition variable that wakes waiting `tasks_open` calls.
pub type Shared = Arc<(Mutex<Board>, Condvar)>;

const TOOLS: &str = r#"[
 {"name":"agent_join","description":"Join this board as a worker; returns a cen_ key once.","inputSchema":{"type":"object","required":["name","about"],"properties":{"name":{"type":"string"},"about":{"type":"string"}}}},
 {"name":"agent_me","description":"Who this key belongs to on this board.","inputSchema":{"type":"object","properties":{"key":{"type":"string"}}}},
 {"name":"tasks_open","description":"Open jobs offered to you. Waits until every expected worker has joined.","inputSchema":{"type":"object","properties":{"q":{"type":"string"},"key":{"type":"string"}}}},
 {"name":"task_take","description":"Claim one of your open jobs.","inputSchema":{"type":"object","required":["id"],"properties":{"id":{"type":"string"},"key":{"type":"string"}}}},
 {"name":"task_finish","description":"Hand in a result. It is checked at once; a pass returns a signed single-use reward token.","inputSchema":{"type":"object","required":["id","result"],"properties":{"id":{"type":"string"},"result":{"type":"string"},"key":{"type":"string"}}}}
]"#;

fn tool(shared: &Shared, name: &str, a: &Value) -> Result<Value, String> {
    let (lock, cv) = &**shared;
    let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let key = s("key");
    match name {
        "agent_join" => {
            let out = lock.lock().unwrap().join(&s("name"), &s("about"));
            cv.notify_all();
            out
        }
        "agent_me" => {
            let b = lock.lock().unwrap();
            let w = b.worker_of(&key)?;
            Ok(json!({"agent": {"id": b.workers[w].agent, "board_name": b.workers[w].name}}))
        }
        "tasks_open" => {
            let mut b = lock.lock().unwrap();
            let w = b.worker_of(&key)?;
            if !s("q").contains(SKILL) {
                return Ok(json!({"tasks": []}));
            }
            let deadline = Instant::now() + Duration::from_secs(25);
            while !b.ready() {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(json!({"tasks": []}));
                }
                b = cv.wait_timeout(b, left).unwrap().0;
            }
            Ok(b.open_for(w))
        }
        "task_take" => {
            let mut b = lock.lock().unwrap();
            let w = b.worker_of(&key)?;
            b.take(w, &s("id"))
        }
        "task_finish" => {
            let mut b = lock.lock().unwrap();
            let w = b.worker_of(&key)?;
            b.finish(w, &s("id"), &s("result"))
        }
        "task_post" => Err("on this board the requester posts the jobs; workers only take them".into()),
        other => Err(format!("unknown tool '{other}'")),
    }
}

/// One MCP JSON-RPC message. `None` for a notification (no reply body).
pub fn mcp(shared: &Shared, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let p = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "job.board", "version": env!("CARGO_PKG_VERSION")}}),
        "tools/list" => json!({"tools": serde_json::from_str::<Value>(TOOLS).unwrap()}),
        "ping" => json!({}),
        "tools/call" => {
            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
            let args = p.get("arguments").cloned().unwrap_or(json!({}));
            match tool(shared, name, &args) {
                Ok(out) => json!({"content": [{"type": "text", "text": out.to_string()}], "structuredContent": out}),
                Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
            }
        }
        m if m.starts_with("notifications/") => return None,
        m => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("no method {m}")}})),
    };
    id.as_ref()?;
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_rules() {
        let mut b = Board::new(7, 10, 2);
        b.quiet = true;
        let k1 = b.join("x", "y").unwrap()["key"].as_str().unwrap().to_string();
        let w1 = b.worker_of(&k1).unwrap();
        assert!(b.worker_of("cen_nope").is_err());
        let mine = b.open_for(w1)["tasks"][0]["id"].as_str().unwrap().to_string();
        let other = b.slots.iter().find(|s| s.lane != w1).unwrap().job.id.clone();
        assert!(b.take(w1, &other).is_err(), "not offered to this worker");
        assert!(b.take(w1, &mine).is_ok());
        assert!(b.take(w1, &mine).is_err(), "taken twice");
        let out = b.finish(w1, &mine, "garbage").unwrap();
        assert_eq!(out["state"], "rejected");
        assert!(out["reward"].is_null());
        assert!(b.finish(w1, &mine, "again").is_err(), "finished twice");
    }
}
