#!/usr/bin/env python3
"""tinyvmm vs Firecracker, side by side: same kernel, same initramfs, same
host (KVM inside the Colima VM, nested on an M4). Run inside the Colima VM,
under the kvm group:

    sg kvm -c "python3 tools/bench.py boot 3"
    sg kvm -c "python3 tools/bench.py clone 3 4"

boot:  process start -> guest /init prints "init is up".
clone: boot a VM whose guest writes '@' to the console nonstop, then
       checkpoint it and start N clones from that checkpoint.
         tinyvmm:     fork (Ctrl-A f on its console)
         Firecracker: pause, snapshot/create (memory file in /dev/shm),
                      resume; each clone = new process + snapshot/load
       Reports how long the original guest was paused, how long until each
       clone's guest is running again (first '@' it prints), and memory.
"""
import glob
import http.client
import json
import os
import re
import shutil
import socket
import statistics
import subprocess
import sys
import threading
import time

HOME = os.environ["HOME"]
FC = f"{HOME}/fc/firecracker"
TINYVMM = f"{HOME}/tinyvmm-target/release/tinyvmm"
KERNEL = f"{HOME}/guest/Image"
INITRD = f"{HOME}/guest/initramfs.cpio"
MEM_MIB = 512
WORK = "/dev/shm/vmm-bench"  # tmpfs, so Firecracker's snapshot files live in RAM


def now_ms():
    return time.monotonic_ns() / 1e6


class Watched:
    """A VMM process; records when given byte patterns first show up in its output."""

    def __init__(self, argv, patterns):
        self.t_spawn = now_ms()
        self.p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.patterns = dict(patterns)
        self.starts = {}  # name -> output offset where matching may begin
        self.seen = {}
        self.out = bytearray()
        self.cv = threading.Condition()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        fd = self.p.stdout.fileno()
        while True:
            chunk = os.read(fd, 1 << 16)
            t = now_ms()
            with self.cv:
                if not chunk:
                    self.seen.setdefault("eof", t)
                    self.cv.notify_all()
                    return
                start = max(0, len(self.out) - 64)
                self.out += chunk
                for name, pat in self.patterns.items():
                    if name not in self.seen and self.out.find(pat, max(start, self.starts.get(name, 0))) >= 0:
                        self.seen[name] = t
                self.cv.notify_all()

    def watch(self, name, pat):
        """Start watching for `pat` in output that arrives from now on."""
        with self.cv:
            self.patterns[name] = pat
            self.starts[name] = len(self.out)
            self.seen.pop(name, None)

    def wait(self, name, timeout=60):
        deadline = time.time() + timeout
        with self.cv:
            while name not in self.seen:
                left = deadline - time.time()
                if left <= 0 or "eof" in self.seen:
                    raise TimeoutError(f"{name!r} not seen; last output: {bytes(self.out[-400:])!r}")
                self.cv.wait(left)
            return self.seen[name]

    def send(self, data):
        self.p.stdin.write(data)
        self.p.stdin.flush()

    def text(self):
        with self.cv:
            return self.out.decode("utf-8", "replace")

    def kill(self):
        try:
            self.p.kill()
        except ProcessLookupError:
            pass
        self.p.wait()


def memory_kib(pid):
    """(Rss, Pss, Private_Dirty) in KiB.
    Rss counts every page the process maps, even ones shared with others, so a
    fork child "has" its parent's whole memory. Private_Dirty is what only this
    process owns: pages it wrote (copy-on-write copies) plus its own heap. That
    is the real extra memory one more clone costs."""
    vals = {}
    with open(f"/proc/{pid}/smaps_rollup") as f:
        for line in f:
            k, _, rest = line.partition(":")
            if k in ("Rss", "Pss", "Private_Dirty"):
                vals[k] = int(rest.split()[0])
    return vals["Rss"], vals["Pss"], vals["Private_Dirty"]


def children_of(pid):
    kids = []
    for stat in glob.glob("/proc/[0-9]*/stat"):
        try:
            fields = open(stat).read().rsplit(")", 1)[1].split()
        except OSError:
            continue
        if int(fields[1]) == pid:
            kids.append(int(stat.split("/")[2]))
    return kids


# --------------------------------------------------------------------------
# Firecracker API (HTTP over a Unix socket)
# --------------------------------------------------------------------------


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost")
        self.unix_path = path

    def connect(self):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(self.unix_path)
        self.sock = s


def fc_api(sock, method, url, body=None):
    c = UnixHTTP(sock)
    c.request(method, url, body=json.dumps(body) if body is not None else None, headers={"Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    c.close()
    if r.status >= 300:
        raise RuntimeError(f"{method} {url} -> {r.status} {data!r}")


def wait_socket(path, timeout=10):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(path)
            s.close()
            return
        except OSError:
            time.sleep(0.0005)
    raise TimeoutError(path)


def fc_config(boot_args):
    return {
        "boot-source": {"kernel_image_path": KERNEL, "initrd_path": INITRD, "boot_args": boot_args},
        "drives": [],
        "machine-config": {"vcpu_count": 1, "mem_size_mib": MEM_MIB},
    }


# --------------------------------------------------------------------------
# Cold boot
# --------------------------------------------------------------------------

BOOT_ARGS = "console=ttyS0 reboot=k panic=-1 rdinit=/init quiet"


def boot_tinyvmm(thp):
    argv = [TINYVMM, "boot", "--mem", str(MEM_MIB), "--cmdline", BOOT_ARGS + " earlycon=uart8250,mmio,0x9000000"]
    if thp:
        argv.append("--thp")
    p = Watched(argv, {"init": b"init is up"})
    t = p.wait("init") - p.t_spawn
    p.send(b"reboot -f\n")
    p.wait("eof")
    p.kill()
    return t


def boot_firecracker():
    os.makedirs(WORK, exist_ok=True)
    cfg = f"{WORK}/boot.json"
    with open(cfg, "w") as f:
        json.dump(fc_config(BOOT_ARGS), f)
    p = Watched([FC, "--no-api", "--config-file", cfg], {"init": b"init is up"})
    t = p.wait("init") - p.t_spawn
    p.send(b"reboot -f\n")
    p.wait("eof")
    p.kill()
    return t


# --------------------------------------------------------------------------
# Checkpoint + clones
# --------------------------------------------------------------------------

HB_ARGS = BOOT_ARGS + " tinyvmm.demo=heartbeat"


def clone_tinyvmm(n, thp):
    argv = [TINYVMM, "boot", "--mem", str(MEM_MIB), "--forks", str(n), "--cmdline", HB_ARGS + " earlycon=uart8250,mmio,0x9000000"]
    if thp:
        argv.append("--thp")
    shutil.rmtree("/tmp/tinyvmm", ignore_errors=True)
    p = Watched(argv, {"hb": b"heartbeat: start"})
    p.wait("hb")
    time.sleep(1.0)  # let it run for a while, like a real workload
    # Measured from outside, like Firecracker: from sending the fork key to
    # the first byte (an '@') in each child's console log.
    t_request = now_ms()
    p.send(b"\x01f")  # Ctrl-A f: fork
    first = {}
    deadline = time.time() + 30
    while len(first) < n:
        if time.time() > deadline:
            raise TimeoutError(p.text()[-2000:])
        for i in range(1, n + 1):
            if i not in first:
                try:
                    if os.path.getsize(f"/tmp/tinyvmm/vm-{i}.log") > 0:
                        first[i] = now_ms() - t_request
                except FileNotFoundError:
                    pass
        time.sleep(0.0002)
    lat = [first[i] for i in range(1, n + 1)]
    # The parent prints its pause line after all forks; a fast child can
    # report first, so wait for both.
    while len(re.findall(r"running ([0-9.]+) ms after the fork request", p.text())) < n or "guest paused" not in p.text():
        time.sleep(0.01)
    txt = p.text()
    paused = float(re.search(r"guest paused ([0-9.]+) ms", txt).group(1))
    time.sleep(1.5)
    kids = children_of(p.p.pid)
    mem = [memory_kib(k) for k in kids]
    parent_mem = memory_kib(p.p.pid)
    p.send(b"\x01x")
    p.wait("eof")
    p.kill()
    return {"paused": paused, "clone_ms": lat, "end_to_end": lat, "clone_mem": mem, "parent_mem": parent_mem}


def clone_firecracker(n):
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK)
    base_sock = f"{WORK}/base.sock"
    cfg = f"{WORK}/hb.json"
    with open(cfg, "w") as f:
        json.dump(fc_config(HB_ARGS), f)
    base = Watched([FC, "--api-sock", base_sock, "--config-file", cfg], {"hb": b"heartbeat: start"})
    base.wait("hb")
    wait_socket(base_sock)
    time.sleep(1.0)

    # Checkpoint: pause, write the snapshot, resume the original.
    snap, mem = f"{WORK}/snap", f"{WORK}/mem"
    t0 = now_ms()
    fc_api(base_sock, "PATCH", "/vm", {"state": "Paused"})
    t1 = now_ms()
    fc_api(base_sock, "PUT", "/snapshot/create", {"snapshot_type": "Full", "snapshot_path": snap, "mem_file_path": mem})
    t2 = now_ms()
    fc_api(base_sock, "PATCH", "/vm", {"state": "Resumed"})
    t3 = now_ms()

    # Clones, all started at once (like tinyvmm's fan-out): new process,
    # wait for its API socket, load the snapshot and resume.
    t_request = now_ms()
    clones, results = [], [None] * n

    def one(i):
        sock = f"{WORK}/clone{i}.sock"
        c = Watched([FC, "--api-sock", sock], {})
        clones.append(c)
        wait_socket(sock)
        c.watch("hb", b"@")
        t_load = now_ms()
        fc_api(sock, "PUT", "/snapshot/load", {"snapshot_path": snap, "mem_backend": {"backend_type": "File", "backend_path": mem}, "resume_vm": True})
        t_loaded = now_ms()
        t_out = c.wait("hb")
        results[i] = {"total": t_out - t_request, "spawn": t_load - t_request, "load_api": t_loaded - t_load, "to_output": t_out - t_loaded}

    threads = [threading.Thread(target=one, args=(i,)) for i in range(n)]
    for th in threads:
        th.start()
    for th in threads:
        th.join()
    time.sleep(1.5)
    clone_mem = [memory_kib(c.p.pid) for c in clones]
    parent_mem = memory_kib(base.p.pid)
    for c in clones + [base]:
        c.kill()
    return {
        "paused": t3 - t0,
        "pause_api": t1 - t0,
        "snapshot_create": t2 - t1,
        "resume_api": t3 - t2,
        "clone_ms": [r["total"] for r in results],
        # checkpoint request -> clone running: snapshot first, then the clone
        "end_to_end": [(t_request - t0) + r["total"] for r in results],
        "clone_detail": results,
        "clone_mem": clone_mem,
        "parent_mem": parent_mem,
        "mem_file_mib": os.path.getsize(mem) / (1 << 20),
        # what the snapshot file really occupies in tmpfs (RAM), holes excluded
        "mem_file_alloc_mib": os.stat(mem).st_blocks * 512 / (1 << 20),
    }


# --------------------------------------------------------------------------


def host_busy():
    """Other heavy work on the host turns a VMM benchmark into a contention
    benchmark. Set BENCH_WAIT_FOR to a docker container-name filter to pause
    trials while matching containers run (unset: never wait)."""
    pattern = os.environ.get("BENCH_WAIT_FOR")
    if not pattern:
        return False
    try:
        out = subprocess.run(["docker", "ps", "-q", "--filter", f"name={pattern}"], capture_output=True, text=True, timeout=20).stdout
    except (OSError, subprocess.TimeoutExpired):
        return False
    return bool(out.strip())


def quiet_trial(fn, label):
    """Run fn only while the host is quiet; redo it if a load test overlapped."""
    while True:
        waited = 0
        while host_busy():
            time.sleep(5)
            waited += 5
        if waited:
            print(f"  (waited {waited} s for the host to go quiet)", flush=True)
        r = fn()
        if not host_busy():
            return r
        print(f"  ({label}: discarded, the host got busy during it)", flush=True)


def interleaved(variants, runs):
    """A, B, C, A, B, C, ...: drift in host load hits every variant alike."""
    results = {name: [] for name, _ in variants}
    for _ in range(runs):
        for name, fn in variants:
            results[name].append(quiet_trial(fn, name))
    return results


def stats(xs):
    xs = sorted(xs)
    return f"min {xs[0]:8.2f}  median {statistics.median(xs):8.2f}  max {xs[-1]:8.2f}"


def main():
    what = sys.argv[1]
    runs = int(sys.argv[2]) if len(sys.argv) > 2 else 3
    if what == "boot":
        variants = [("tinyvmm (4K pages)", lambda: boot_tinyvmm(False)), ("tinyvmm (--thp)", lambda: boot_tinyvmm(True)), ("Firecracker", boot_firecracker)]
        for name, ts in interleaved(variants, runs).items():
            print(f"cold boot to init  {name:<20} {stats(ts)} ms   runs: {[round(t) for t in ts]}", flush=True)
    elif what == "clone":
        n = int(sys.argv[3]) if len(sys.argv) > 3 else 1
        variants = [("tinyvmm (4K pages)", lambda: clone_tinyvmm(n, False)), ("tinyvmm (--thp)", lambda: clone_tinyvmm(n, True)), ("Firecracker", lambda: clone_firecracker(n))]
        for name, rs in interleaved(variants, runs).items():
            print(f"\n== {name}, {n} clone(s), {runs} runs", flush=True)
            print(f"  original guest paused (ms)      {stats([r['paused'] for r in rs])}")
            if "snapshot_create" in rs[0]:
                print(f"    of which snapshot/create      {stats([r['snapshot_create'] for r in rs])}   memory file {rs[0]['mem_file_mib']:.0f} MiB")
            print(f"  request -> clone running (ms)   {stats([x for r in rs for x in r['clone_ms']])}")
            print(f"  request -> last clone running   {stats([max(r['clone_ms']) for r in rs])}")
            print(f"  checkpoint request -> clone running (end to end) {stats([x for r in rs for x in r['end_to_end']])}")
            if "clone_detail" in rs[0]:
                d = [x for r in rs for x in r["clone_detail"]]
                print(f"    spawn process + API ready     {stats([x['spawn'] for x in d])}")
                print(f"    snapshot/load call            {stats([x['load_api'] for x in d])}")
                print(f"    loaded -> first guest output  {stats([x['to_output'] for x in d])}")
            rss = [m[0] / 1024 for r in rs for m in r["clone_mem"]]
            pss = [m[1] / 1024 for r in rs for m in r["clone_mem"]]
            own = [m[2] / 1024 for r in rs for m in r["clone_mem"]]
            print(f"  per clone RSS (MiB)             {stats(rss)}")
            print(f"  per clone private dirty (MiB)   {stats(own)}")
            # Host RAM for the original + n clones. tinyvmm: the original's
            # memory is shared copy-on-write, each clone adds only what it
            # wrote. Firecracker: the original keeps its own memory, the
            # snapshot file sits in tmpfs, each clone adds what it wrote.
            totals = []
            for r in rs:
                t = r["parent_mem"][0] / 1024 + sum(m[2] for m in r["clone_mem"]) / 1024
                t += r.get("mem_file_alloc_mib", 0)
                totals.append(t)
            extra = f"  (incl. snapshot file {rs[-1]['mem_file_alloc_mib']:.0f} MiB in tmpfs)" if "mem_file_alloc_mib" in rs[-1] else ""
            print(f"  host RAM, original + {n} clone(s) {stats(totals)} MiB{extra}")
            print(f"  original RSS (MiB)              {rs[-1]['parent_mem'][0] / 1024:.0f}", flush=True)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
