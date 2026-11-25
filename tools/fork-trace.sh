#!/bin/sh
# Run inside the Colima VM. Boots the fork demo with KVM tracing on, then
# breaks down what each child's vCPU did between "resumed" and "guest read
# its role": how many exits of each kind KVM handled in the kernel, and how
# the time split between running guest code and handling exits.
# Usage: fork-trace.sh [children] [extra tinyvmm args...]
N=${1:-1}; shift 2>/dev/null
B=$HOME/tinyvmm-target/release/tinyvmm
T=/sys/kernel/tracing
sudo sh -c "echo 0 > $T/tracing_on; echo mono > $T/trace_clock; echo 32768 > $T/buffer_size_kb; echo > $T/trace
            echo 1 > $T/events/kvm/kvm_exit/enable; echo 1 > $T/events/kvm/kvm_entry/enable; echo 1 > $T/tracing_on"
rm -rf /tmp/tinyvmm
(sleep 30) | TINYVMM_TRACE=1 timeout 90 sg kvm -c "TINYVMM_TRACE=1 $B boot --thp --append 'quiet tinyvmm.demo=fork' --forks $N $*" 2>&1 | tr -d '\r' > /tmp/fork-trace.out
sudo sh -c "echo 0 > $T/tracing_on; echo 0 > $T/events/kvm/kvm_exit/enable; echo 0 > $T/events/kvm/kvm_entry/enable"
sudo cat $T/trace > /tmp/fork-trace.raw
grep -E "guest paused|running .* after the fork" /tmp/fork-trace.out
grep "trace window" /tmp/fork-trace.out | while read -r _ _ name _ _ _ pid _ start end; do
  awk -v pid="$pid" -v s="$start" -v e="$end" -v name="${name%]}" '
    /kvm_(exit|entry)/ {
      split($1, a, "-"); if (a[length(a)] != pid) next
      t = $4; sub(":", "", t); t += 0; if (t < s || t > e) next
      if ($5 == "kvm_entry:") { if (last_exit) handle += t - last_exit; last_entry = t; entries++ }
      else {
        if (last_entry) inguest += t - last_entry
        last_exit = t
        k = ($6 == "IRQ:") ? "host IRQ" : $9
        n[k]++
      }
    }
    END {
      printf "vm %s: window %.2f ms = guest running %.2f ms + KVM handling exits %.2f ms, %d entries\n", name, (e-s)*1000, inguest*1000, handle*1000, entries
      for (k in n) printf "    %-12s %6d\n", k, n[k]
    }' /tmp/fork-trace.raw
done
