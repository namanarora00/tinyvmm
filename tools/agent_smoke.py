"""Does the agent VM work? Boot, exec, fork, isolation between forks.
Run inside the Colima VM under the kvm group:
    sg kvm -c "python3 tools/agent_smoke.py"
"""
import shutil
import sys
import time

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from rlenv import TinyVM

D = "/tmp/rlenv-smoke"
shutil.rmtree(D, ignore_errors=True)
log = open("/tmp/rlenv-smoke.log", "w")

vm = TinyVM.boot(D, log=log)
print(f"booted: agent ready {vm.boot_ms:.0f} ms after process start")

r = vm.exec("python3 -c 'import sys, platform; print(sys.version.split()[0], platform.machine())'")
print(f"exec: {r['output'].strip()!r} exit {r['exit']} ({r['ms']:.1f} ms inside the guest)")

t = time.monotonic()
for _ in range(20):
    vm.exec("true")
print(f"round trip, empty command: {(time.monotonic() - t) / 20 * 1000:.1f} ms")

big = "x" * 200_000
t = time.monotonic()
r = vm.exec("wc -c < big.txt", files={"big.txt": big})
print(f"200 KB file in + command: {(time.monotonic() - t) * 1000:.1f} ms, guest saw {r['output'].strip()} bytes")

# State the children should inherit: a file, and a running process.
vm.exec("echo prepared > state.txt; (while true; do date +%s%N > tick; sleep 0.05; done) >/dev/null 2>&1 &")
time.sleep(0.3)

t = time.monotonic()
kids = vm.fork(3)
fork_ms = (time.monotonic() - t) * 1000
print(f"fork 3: {fork_ms:.1f} ms until all 3 children listen ({[k.name for k in kids]})")

for i, k in enumerate(kids):
    r = k.exec(f"cat state.txt; echo child{i} > mine.txt; ls")
    print(f"  child {k.name}: {r['output'].split()}")
r = vm.exec("ls; cat mine.txt 2>&1 || true")
print(f"  parent after children wrote: {r['output'].split()}")
a, b = (kids[0].exec("cat tick")["output"].strip(), kids[0].exec("sleep 0.2; cat tick")["output"].strip())
print(f"  background process still running in child: {a != b}")

for k in kids:
    k.quit()
vm.quit()
print("all quit cleanly")
