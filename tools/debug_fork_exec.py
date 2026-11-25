"""Why are commands slow in fork children? Time the same command in the
template and in children, one child at a time and all at once."""
import shutil, sys, time, threading
sys.path.insert(0, __file__.rsplit("/", 1)[0]); sys.path.insert(0, "/Users/naman/Code/minirl")
from minirl.tasks.code import RUNNER, load_tasks
from rlenv import TinyVM
task = load_tasks()[0]; cwd = f"/work/{task['id']}"
D = "/tmp/rlbench-dbg"; shutil.rmtree(D, ignore_errors=True)
THP = "--no-thp" not in sys.argv
vm = TinyVM.boot(D, log=open("/tmp/rlbench-dbg.log", "w"), thp=THP)
import subprocess; print("THP", THP, subprocess.run(["grep", "-E", "AnonHugePages|^Rss", f"/proc/{vm.proc.pid}/smaps_rollup"], capture_output=True, text=True).stdout.split())
vm.exec(files={"tests.py": task["test_hidden"], "run_tests.py": RUNNER}, cwd=cwd)
def timed(v, label):
    t = time.monotonic(); r = v.exec("python3 run_tests.py tests", files={"solution.py": task["original"]}, cwd=cwd, timeout=60)
    print(f"{label:<22} round trip {(time.monotonic()-t)*1000:8.1f} ms  in-guest {r['ms']:8.1f} ms  exit {r['exit']}  {r['output'].strip().splitlines()[-1][:40]!r}", flush=True)
for i in range(3): timed(vm, f"template run {i}")
k = vm.fork(1)[0]
for i in range(3): timed(k, f"single child run {i}")
k.quit()
kids = vm.fork(8)
ts = [threading.Thread(target=timed, args=(c, f"child {c.name} (8 at once)")) for c in kids]
[t.start() for t in ts]; [t.join() for t in ts]
for c in kids: timed(c, f"child {c.name} 2nd run")
for c in kids: c.quit()
vm.quit()
