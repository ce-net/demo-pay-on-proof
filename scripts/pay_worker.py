#!/usr/bin/env python3
"""A pay-on-proof worker: the public task-worker-loop worker.py, unchanged, given a skill.

  python3 scripts/pay_worker.py [--cheat] [--pace SECONDS]

worker.py (from github.com/ce-net/ce-net-task-worker-loop, found through WORKER_LOOP_DIR) does
the joining, polling, taking and finishing. This file only supplies do_task() for the three job
kinds. --cheat makes it the rigged worker: on about 10% of jobs (picked by CHEAT_SEED) it returns
a wrong answer and appends the job to CHEAT_LEDGER, so the run can be audited afterwards.
"""
import hashlib, json, os, sys, time, urllib.request

argv = sys.argv[1:]
CHEAT = "--cheat" in argv
PACE = float(argv[argv.index("--pace") + 1]) if "--pace" in argv else 0.0
SEED = int(os.environ.get("CHEAT_SEED", "68"))
RATE = 0.10
LEDGER = os.environ.get("CHEAT_LEDGER", "")

os.environ.setdefault("CE_SKILL", "pay-on-proof")
sys.path.insert(0, os.environ["WORKER_LOOP_DIR"])
import worker  # noqa: E402  the unchanged public worker loop

BASE = worker.URL.rsplit("/mcp", 1)[0]


def injected(job_id):
    """Is this one of the jobs the rigged worker gets wrong? About RATE of them, fixed by the seed."""
    d = hashlib.sha256(f"pay-on-proof cheat|{SEED}|{job_id}".encode()).digest()
    return int.from_bytes(d[:8], "big") < RATE * 2**64


def corpus(off, length):
    with urllib.request.urlopen(f"{BASE}/corpus?off={off}&len={length}", timeout=30) as r:
        return r.read()


def solve(a, b):
    """Gaussian elimination with partial pivoting."""
    n = len(b)
    m = [list(map(float, row)) + [float(bi)] for row, bi in zip(a, b)]
    for c in range(n):
        p = max(range(c, n), key=lambda r: abs(m[r][c]))
        m[c], m[p] = m[p], m[c]
        for r in range(c + 1, n):
            f = m[r][c] / m[c][c]
            for k in range(c, n + 1):
                m[r][k] -= f * m[c][k]
    x = [0.0] * n
    for r in range(n - 1, -1, -1):
        t = 0.0  # a plain left-to-right loop: sum() rounds differently from Python 3.12 on
        for k in range(r + 1, n):
            t += m[r][k] * x[k]
        x[r] = (m[r][n] - t) / m[r][r]
    return x


def answer(spec, wrong):
    kind = spec["kind"]
    if kind == "hash":
        off, blocks, block = spec["off"], spec["blocks"], spec["block"]
        if wrong:
            off += 1  # hash the range one byte late: cheaper to fake than to get right
        data = corpus(off, blocks * block)
        return [hashlib.sha256(data[i * block:(i + 1) * block]).hexdigest() for i in range(blocks)], "range shifted by one byte"
    if kind == "sort":
        out = sorted(spec["values"])
        if wrong:
            k = len(out) // 2
            out[k] = out[k - 1]  # still in order, but one number replaced by its neighbour
        return out, "one number replaced by its neighbour"
    if kind == "solve":
        x = solve(spec["a"], spec["b"])
        if wrong:
            x[0] += 1e-3 * (1 + abs(x[0]))  # close, but not a solution
        return x, "first unknown off by 0.1%"
    raise ValueError(f"unknown job kind {kind}")


def do_task(need):
    job_id = need.split()[2].rstrip(":")
    spec = json.loads(need[need.index("{"):])
    wrong = CHEAT and injected(job_id)
    result, how = answer(spec, wrong)
    if wrong and LEDGER:
        with open(LEDGER, "a") as f:
            f.write(json.dumps({"job": job_id, "kind": spec["kind"], "how": how}) + "\n")
    if PACE:
        time.sleep(PACE)
    return json.dumps(result, separators=(",", ":"))


worker.do_task = do_task

if __name__ == "__main__":
    sys.exit(worker.main(["worker.py", "work"]))
