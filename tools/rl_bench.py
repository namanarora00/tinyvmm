"""Grade RL rollout groups three ways and compare: tinyvmm forks vs Docker.

For each task (bug-fixing tasks from ~/Code/minirl), a GRPO-style group of G
candidate fixes is graded by the task's hidden tests:
  candidate 0 = the correct code (reward 1), the rest = the buggy file and
  other wrong mutants of the same function (reward 0). Known answers, so
  every backend's rewards can be checked, and no model has to run.

Backends:
  tinyvmm      one template VM, booted once. Per task: write the test files
               into the template ("environment setup"), fork G children, run
               one candidate in each, quit them. Fresh, isolated copy per
               episode, processes and all.
  docker-fresh one new container per candidate (docker run --rm): the same
               per-episode isolation, the usual way RL environments are run.
  docker-pool  8 long-lived containers reused across candidates: fast, but
               episodes share a container (one can disturb the next).

Run inside the Colima VM:  sg kvm -c "python3 tools/rl_bench.py 8"
"""

import glob
import io
import re
import os
import shutil
import statistics
import subprocess
import sys
import tarfile
import threading
import time

sys.path.insert(0, __file__.rsplit("/", 1)[0])
sys.path.insert(0, os.environ.get("MINIRL", "/Users/naman/Code/minirl"))
from minirl.tasks.code import RUNNER, load_tasks  # noqa: E402
from rlenv import TinyVM  # noqa: E402

G = 8
IMAGE = "python:3.12-slim"
TEST_CMD = "python3 run_tests.py tests"


def reward(output):
    m = re.search(r"RESULT (\d+)/(\d+)", output)
    return 1.0 if m and m.group(1) == m.group(2) and int(m.group(2)) > 0 else 0.0


def groups(n_tasks):
    """One task per seed function: (task, [G candidate solutions], expected rewards)."""
    tasks = load_tasks()
    by_seed = {}
    for t in tasks:
        by_seed.setdefault(t["seed"], []).append(t)
    out = []
    for seed, ts in list(by_seed.items())[:n_tasks]:
        task = ts[0]
        wrong = [t["buggy"] for t in ts]
        cands = [task["original"]] + [wrong[i % len(wrong)] for i in range(G - 1)]
        out.append((task, cands, [1.0] + [0.0] * (G - 1)))
    return out


def private_dirty_mib(pid):
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Private_Dirty:"):
                    return int(line.split()[1]) / 1024
    except OSError:
        pass  # exited (an earlier group's child not reaped yet)
    return None


def parallel(fn, items):
    results = [None] * len(items)
    threads = [threading.Thread(target=lambda i=i, x=x: results.__setitem__(i, fn(x))) for i, x in enumerate(items)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return results


# --- tinyvmm ---------------------------------------------------------------


def run_tinyvmm(gs):
    d = "/tmp/rlbench-vm"
    shutil.rmtree(d, ignore_errors=True)
    log = open("/tmp/rlbench-vm.log", "w")
    t = time.monotonic()
    vm = TinyVM.boot(d, log=log)
    boot_s = time.monotonic() - t
    rows = []
    for task, cands, _ in gs:
        cwd = f"/work/{task['id']}"
        t0 = time.monotonic()
        vm.exec(files={"tests.py": task["test_hidden"], "run_tests.py": RUNNER}, cwd=cwd)  # environment setup
        t1 = time.monotonic()
        kids = vm.fork(G)
        t2 = time.monotonic()
        outs = parallel(lambda kc: kc[0].exec(TEST_CMD, files={"solution.py": kc[1]}, cwd=cwd, timeout=120)["output"], list(zip(kids, cands)))
        t3 = time.monotonic()
        kid_pids = [p for p in children_pids(vm.proc.pid)]
        mem = [m for m in (private_dirty_mib(p) for p in kid_pids) if m is not None]
        for k in kids:
            k.quit()
        rows.append({"task": task["id"], "rewards": [reward(o) for o in outs], "setup_ms": (t1 - t0) * 1000,
                     "fork_ms": (t2 - t1) * 1000, "run_ms": (t3 - t2) * 1000, "total_ms": (t3 - t1) * 1000,
                     "mem_mib": mem})
    template_mib = private_dirty_mib(vm.proc.pid)
    vm.quit()
    log.close()
    paused = []
    for x in re.findall(r"guest paused ([0-9.]+) ms", open("/tmp/rlbench-vm.log", errors="replace").read()):
        try:
            paused.append(float(x))
        except ValueError:
            pass  # child processes share the log; a line can come out interleaved
    return {"boot_s": boot_s, "rows": rows, "template_mib": template_mib, "paused_ms": paused}


def children_pids(pid):
    kids = []
    for stat in glob.glob("/proc/[0-9]*/stat"):
        try:
            fields = open(stat).read().rsplit(")", 1)[1].split()
        except OSError:
            continue
        if int(fields[1]) == pid:
            kids.append(int(stat.split("/")[2]))
    return kids


# --- Docker ----------------------------------------------------------------


def tar_of(files):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        for path, text in files.items():
            data = text.encode()
            info = tarfile.TarInfo(path)
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def docker_fresh_one(files):
    # New container per episode; also report the container's peak memory.
    script = f"mkdir -p /w && cd /w && tar -x && {TEST_CMD}; echo; echo PEAK $(cat /sys/fs/cgroup/memory.peak 2>/dev/null)"
    p = subprocess.run(["docker", "run", "--rm", "-i", "--network", "none", "--memory", "256m", IMAGE, "sh", "-c", script],
                       input=tar_of(files), capture_output=True)
    out = p.stdout.decode(errors="replace")
    m = re.search(r"PEAK (\d+)", out)
    return out, (int(m.group(1)) / (1 << 20) if m else 0.0)


def run_docker_fresh(gs):
    rows = []
    for task, cands, _ in gs:
        base = {"tests.py": task["test_hidden"], "run_tests.py": RUNNER}
        t0 = time.monotonic()
        res = parallel(lambda c: docker_fresh_one({**base, "solution.py": c}), cands)
        rows.append({"task": task["id"], "rewards": [reward(o) for o, _ in res], "total_ms": (time.monotonic() - t0) * 1000,
                     "mem_mib": [m for _, m in res]})
    return {"rows": rows}


def run_docker_pool(gs):
    from minirl.sandbox.docker import DockerPool

    t = time.monotonic()
    pool = DockerPool(G)
    start_s = time.monotonic() - t
    rows = []
    for task, cands, _ in gs:
        base = {"tests.py": task["test_hidden"], "run_tests.py": RUNNER}
        t0 = time.monotonic()
        outs = parallel(lambda c: pool.run({**base, "solution.py": c}, TEST_CMD)[1], cands)
        rows.append({"task": task["id"], "rewards": [reward(o) for o in outs], "total_ms": (time.monotonic() - t0) * 1000})
    pool.close()
    return {"rows": rows, "start_s": start_s}


# ---------------------------------------------------------------------------


def stats(xs):
    xs = sorted(xs)
    return f"median {statistics.median(xs):8.1f}   min {xs[0]:8.1f}   max {xs[-1]:8.1f}"


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 8
    gs = groups(n)
    expected = [e for _, _, e in gs]
    print(f"{len(gs)} tasks x {G} candidates each (candidate 0 correct, the rest wrong)\n", flush=True)

    vm = run_tinyvmm(gs)
    fresh = run_docker_fresh(gs)
    pool = run_docker_pool(gs)

    for name, res in (("tinyvmm", vm), ("docker-fresh", fresh), ("docker-pool", pool)):
        ok = all(r["rewards"] == e for r, e in zip(res["rows"], expected))
        print(f"{name:<13} rewards match the known answers: {ok}")
    print()
    print(f"one-time start: tinyvmm boot {vm['boot_s']:.1f} s (template VM), docker-pool {pool['start_s']:.1f} s ({G} containers)")
    print(f"per group of {G} (ms):")
    print(f"  tinyvmm       fork {G} + run {G}   {stats([r['total_ms'] for r in vm['rows']])}")
    print(f"                  of which fork      {stats([r['fork_ms'] for r in vm['rows']])}")
    print(f"                  of which run       {stats([r['run_ms'] for r in vm['rows']])}")
    print(f"                env setup in template {stats([r['setup_ms'] for r in vm['rows']])}")
    print(f"                template guest paused per fork {stats(vm['paused_ms'])}")
    print(f"  docker-fresh  {G} x docker run    {stats([r['total_ms'] for r in fresh['rows']])}")
    print(f"  docker-pool   {G} x docker exec   {stats([r['total_ms'] for r in pool['rows']])}")
    print("memory per episode (MiB):")
    print(f"  tinyvmm child, private (not shared with template)  {stats([m for r in vm['rows'] for m in r['mem_mib']])}")
    print(f"  tinyvmm template VMM process, private               {vm['template_mib']:.0f}")
    print(f"  docker-fresh container, cgroup peak                 {stats([m for r in fresh['rows'] for m in r['mem_mib']])}")


if __name__ == "__main__":
    main()
