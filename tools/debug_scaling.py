"""First command in N fresh fork children at once: how does it scale, and
where does the CPU time go (Colima VM /proc/stat: user, system, idle, guest)?"""
import os
import shutil, sys, time, threading
sys.path.insert(0, __file__.rsplit("/", 1)[0]); sys.path.insert(0, os.environ.get("MINIRL", "/Users/naman/Code/minirl"))
from minirl.tasks.code import RUNNER, load_tasks
from rlenv import TinyVM
task = load_tasks()[0]; cwd = f"/work/{task['id']}"
D = "/tmp/rlbench-scale"; shutil.rmtree(D, ignore_errors=True)
vm = TinyVM.boot(D, log=open("/tmp/rlbench-scale.log", "w"))
vm.exec(files={"tests.py": task["test_hidden"], "run_tests.py": RUNNER}, cwd=cwd)
def cpu():
    f = open("/proc/stat").readline().split()[1:]
    return dict(zip(["user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal", "guest", "guest_nice"], map(int, f)))
for n in (1, 2, 4, 8):
    kids = vm.fork(n)
    times = []
    def run(c):
        t = time.monotonic(); c.exec("python3 run_tests.py tests", files={"solution.py": task["original"]}, cwd=cwd, timeout=120); times.append(time.monotonic() - t)
    c0, t0 = cpu(), time.monotonic()
    ts = [threading.Thread(target=run, args=(c,)) for c in kids]; [t.start() for t in ts]; [t.join() for t in ts]
    c1, wall = cpu(), time.monotonic() - t0
    d = {k: (c1[k] - c0[k]) / 100 for k in c0}  # USER_HZ=100 -> seconds across all 8 CPUs
    print(f"{n} at once: each {sum(times)/n*1000:7.0f} ms (wall {wall*1000:6.0f} ms) | CPU-seconds: guest {d['guest']:.1f} user {d['user']-d['guest']:.1f} system {d['system']:.1f} idle {d['idle']:.1f} steal {d['steal']:.1f}", flush=True)
    for c in kids: c.quit()
vm.quit()
