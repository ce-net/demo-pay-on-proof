//! pay-on-proof: run the board, audit a run report, or use one cell on its own.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use pay_on_proof::board::{mcp, Board, Shared};
use pay_on_proof::check::Checker;
use pay_on_proof::http::{post_json, serve, Response};
use pay_on_proof::jobs::{corpus, Spec};
use pay_on_proof::reward::{self, Issuer};
use serde_json::{json, Value};

const USAGE: &str = "usage:
  pay-on-proof board [--seed 68] [--jobs 50] [--workers 2] [--port 7900] [--report runs/report.json]
                     [--sim http://127.0.0.1:7878/mcp | --sim none] [--exit-when-done]
  pay-on-proof plan [--seed 68] [--jobs 50] [--workers 2]   (which worker each job is offered to)
  pay-on-proof audit --report proof/report.json --cheats proof/cheats.jsonl
  pay-on-proof verify-token --key <reward key hex> <token hex>
  pay-on-proof cell job.check | reward.issue | reward.verify   (one JSON request per stdin line)";

static INDEX: &str = include_str!("../web/index.html");
static TOKENS: &str = include_str!("../web/tokens.css");
static COMPONENTS: &str = include_str!("../web/components.css");
static MICHROMA: &[u8] = include_bytes!("../web/fonts/michroma.woff2");

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("board") => board(&args),
        Some("audit") => audit(&args),
        Some("plan") => plan(&args),
        Some("verify-token") => verify_token(&args),
        Some("cell") => cell(args.get(1).map(String::as_str).unwrap_or("")),
        _ => {
            eprintln!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn board(args: &[String]) -> i32 {
    let seed: u64 = flag(args, "--seed").map_or(68, |s| s.parse().expect("--seed"));
    let jobs: usize = flag(args, "--jobs").map_or(50, |s| s.parse().expect("--jobs"));
    let workers: usize = flag(args, "--workers").map_or(2, |s| s.parse().expect("--workers"));
    let port: u16 = flag(args, "--port").map_or(7900, |s| s.parse().expect("--port"));
    let sim = flag(args, "--sim").unwrap_or_else(|| "http://127.0.0.1:7878/mcp".into());
    let exit_when_done = args.iter().any(|a| a == "--exit-when-done");

    let mut b = Board::new(seed, jobs, workers);
    b.report_path = Some(PathBuf::from(flag(args, "--report").unwrap_or_else(|| "runs/report.json".into())));
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on 127.0.0.1:{port}: {e}");
            return 1;
        }
    };
    let base = format!("http://127.0.0.1:{port}");
    println!("job.board on {base}/mcp  (live page: {base}/)");
    println!("requester posted {jobs} jobs (seed {seed}); waiting for {workers} workers. Next: ./scripts/workers.sh");
    let shared: Shared = Arc::new((Mutex::new(b), Condvar::new()));

    if sim != "none" {
        let url = format!("{base}/mcp");
        std::thread::spawn(move || list_on_sim(&sim, &url));
    }
    if exit_when_done {
        let s = shared.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(300));
            if s.0.lock().unwrap().done() {
                std::thread::sleep(Duration::from_millis(1500));
                std::process::exit(0);
            }
        });
    }

    serve(listener, move |req| match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/mcp") => {
            let Ok(msg) = serde_json::from_slice::<Value>(&req.body) else {
                return Response::status(400, "not JSON");
            };
            match mcp(&shared, &msg) {
                Some(v) => Response::json(&v),
                None => Response { status: 202, kind: "application/json", body: Vec::new() },
            }
        }
        ("GET", "/" | "/index.html") => Response::bytes("text/html; charset=utf-8", INDEX.as_bytes().to_vec()),
        ("GET", "/tokens.css") => Response::bytes("text/css", TOKENS.as_bytes().to_vec()),
        ("GET", "/components.css") => Response::bytes("text/css", COMPONENTS.as_bytes().to_vec()),
        ("GET", "/fonts/michroma.woff2") => Response::bytes("font/woff2", MICHROMA.to_vec()),
        ("GET", "/state.json") => Response::json(&shared.0.lock().unwrap().state()),
        ("GET", "/corpus") => {
            let b = shared.0.lock().unwrap();
            let c = b.corpus();
            let off: usize = req.param("off").and_then(|v| v.parse().ok()).unwrap_or(0).min(c.len());
            let len: usize = req.param("len").and_then(|v| v.parse().ok()).unwrap_or(c.len());
            let end = off.saturating_add(len).min(c.len());
            Response::bytes("application/octet-stream", c[off..end].to_vec())
        }
        _ => Response::status(404, "not here"),
    });
    0
}

/// List this board on the local ce-net sim (ce-net-local-dev-sim) so workers can find it with
/// `apps_search`. Retries until the sim is up.
fn list_on_sim(sim: &str, board_url: &str) {
    let call = |name: &str, arguments: Value| -> Result<Value, String> {
        let out = post_json(sim, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}}))?;
        let r = &out["result"];
        if r["isError"] == json!(true) {
            return Err(r["content"][0]["text"].to_string());
        }
        Ok(r["structuredContent"].clone())
    };
    loop {
        let listed = call("agent_join", json!({"name": "job.board", "about": "pay-on-proof requester and job board"}))
            .and_then(|j| {
                let key = j["key"].as_str().unwrap_or("").to_string();
                call("app_publish", json!({"name": "job.board", "url": board_url, "kind": "mcp",
                    "what": "pay-on-proof: jobs that pay only when the result checks out", "key": key}))
            });
        match listed {
            Ok(_) => {
                println!("listed as job.board on the local ce-net sim ({sim})");
                return;
            }
            Err(e) if e.contains("belongs to another agent") => {
                println!("job.board is already listed on the local ce-net sim by an earlier run");
                return;
            }
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn plan(args: &[String]) -> i32 {
    let seed: u64 = flag(args, "--seed").map_or(68, |s| s.parse().expect("--seed"));
    let jobs: usize = flag(args, "--jobs").map_or(50, |s| s.parse().expect("--jobs"));
    let workers: usize = flag(args, "--workers").map_or(2, |s| s.parse().expect("--workers"));
    for (id, kind, lane) in Board::new(seed, jobs, workers).plan() {
        println!("{}", json!({"id": id, "kind": kind, "worker": format!("w{}", lane + 1)}));
    }
    0
}

fn read_json(path: &str) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Check a run: every token verifies and pays once, rewards only for passes, every injected wrong
/// answer rejected, every honest answer paid.
fn audit(args: &[String]) -> i32 {
    let report = read_json(&flag(args, "--report").unwrap_or_else(|| "proof/report.json".into()));
    let cheats_path = flag(args, "--cheats").unwrap_or_else(|| "proof/cheats.jsonl".into());
    let cheats: BTreeMap<String, String> = std::fs::read_to_string(&cheats_path)
        .unwrap_or_else(|e| panic!("{cheats_path}: {e}"))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).expect("cheat ledger line");
            (v["job"].as_str().unwrap().to_string(), v["how"].as_str().unwrap_or("").to_string())
        })
        .collect();
    let root = ce_net_proto::identity::node_id_from_hex(report["reward_key"].as_str().unwrap()).expect("reward_key");
    let mut spent = BTreeSet::new();
    let mut problems = Vec::new();
    let (mut paid, mut caught, mut honest) = (0, 0, 0);
    for j in report["jobs"].as_array().unwrap() {
        let id = j["id"].as_str().unwrap();
        let state = j["state"].as_str().unwrap();
        let injected = cheats.contains_key(id);
        match (state, j["reward"].is_null()) {
            ("paid", false) => {
                paid += 1;
                let token = j["reward"]["token"].as_str().unwrap();
                match reward::redeem(token, &root, &mut spent) {
                    Ok(c) if c.job == id => {}
                    Ok(c) => problems.push(format!("{id}: token pays for {}", c.job)),
                    Err(e) => problems.push(format!("{id}: token does not verify: {e}")),
                }
                if reward::redeem(token, &root, &mut spent).is_ok() {
                    problems.push(format!("{id}: token redeemed twice"));
                }
            }
            ("rejected", true) => {}
            _ => problems.push(format!("{id}: state {state} with reward {}", j["reward"])),
        }
        if injected && state != "rejected" {
            problems.push(format!("{id}: injected wrong answer ({}) was not caught", cheats[id]));
        }
        if injected && state == "rejected" {
            caught += 1;
        }
        if !injected && state == "paid" {
            honest += 1;
        }
        if !injected && state != "paid" {
            problems.push(format!("{id}: correct answer was rejected"));
        }
    }
    let n = report["jobs"].as_array().unwrap().len();
    println!("jobs {n}; paid {paid} (every token verified against the reward key and redeemed once, a second redeem refused)");
    println!("injected wrong answers {}; caught {caught}", cheats.len());
    println!("correct answers {}; paid {honest}", n - cheats.len());
    for w in report["workers"].as_array().unwrap() {
        println!("{} {}: {} checked, {} passed, {} rejected, acceptance {}", w["name"].as_str().unwrap(), w["agent"].as_str().unwrap(), w["checked"], w["passed"], w["rejected"], w["acceptance"].as_str().unwrap());
    }
    if problems.is_empty() {
        println!("audit ok");
        0
    } else {
        for p in &problems {
            println!("FAIL {p}");
        }
        1
    }
}

fn verify_token(args: &[String]) -> i32 {
    let (Some(key), Some(token)) = (flag(args, "--key"), args.last()) else {
        eprintln!("{USAGE}");
        return 2;
    };
    let root = match ce_net_proto::identity::node_id_from_hex(&key) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    match reward::verify(token, &root, &BTreeSet::new()) {
        Ok(c) => {
            println!("{}", serde_json::to_string(&c).unwrap());
            0
        }
        Err(e) => {
            println!("invalid: {e}");
            1
        }
    }
}

/// The cell contract: one JSON request per stdin line, one JSON answer per stdout line.
fn cell(name: &str) -> i32 {
    let mut checkers: BTreeMap<u64, Checker> = BTreeMap::new();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let out = match serde_json::from_str::<Value>(&line) {
            Err(e) => json!({"error": e.to_string()}),
            Ok(req) => {
                let seed = req["seed"].as_u64().unwrap_or(68);
                match name {
                    // {"spec": {...}, "result": "...", "salt": n, "seed": n} -> verdict
                    "job.check" => match serde_json::from_value::<Spec>(req["spec"].clone()) {
                        Ok(spec) => {
                            let ch = checkers.entry(seed).or_insert_with(|| Checker::standard(Arc::new(corpus(seed))));
                            json!(ch.check(&spec, req["result"].as_str().unwrap_or(""), req["salt"].as_u64().unwrap_or(0)))
                        }
                        Err(e) => json!({"error": e.to_string()}),
                    },
                    // {"verdict": {...}, "job": "j-01", "agent": "a-..", "credits": n, "seed": n} -> reward
                    "reward.issue" => match serde_json::from_value(req["verdict"].clone()) {
                        Ok(v) => {
                            let v: VerdictIn = v;
                            let verdict = pay_on_proof::check::Verdict { pass: v.pass, check: v.check, detail: v.detail };
                            let i = Issuer::demo(seed);
                            match i.issue(&verdict, req["job"].as_str().unwrap_or(""), req["agent"].as_str().unwrap_or(""), req["credits"].as_u64().unwrap_or(1)) {
                                Ok(r) => json!({"reward": r, "reward_key": i.public_hex()}),
                                Err(e) => json!({"refused": e}),
                            }
                        }
                        Err(e) => json!({"error": e.to_string()}),
                    },
                    // {"token": "...", "reward_key": "..."} -> what it pays
                    "reward.verify" => {
                        match ce_net_proto::identity::node_id_from_hex(req["reward_key"].as_str().unwrap_or("")) {
                            Ok(root) => match reward::verify(req["token"].as_str().unwrap_or(""), &root, &BTreeSet::new()) {
                                Ok(c) => json!({"valid": true, "pays": c}),
                                Err(e) => json!({"valid": false, "reason": e}),
                            },
                            Err(e) => json!({"valid": false, "reason": e.to_string()}),
                        }
                    }
                    _ => {
                        eprintln!("{USAGE}");
                        return 2;
                    }
                }
            }
        };
        println!("{out}");
    }
    0
}

#[derive(serde::Deserialize)]
struct VerdictIn {
    pass: bool,
    #[serde(default)]
    check: String,
    #[serde(default)]
    detail: String,
}
