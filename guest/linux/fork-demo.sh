#!/bin/sh
# Runs as PID 1 when the kernel command line has tinyvmm.demo=fork.
# Checks that a fork keeps a *running* Linux intact: processes keep going,
# files survive, timers and the console still work, and each copy diverges.

# A background process whose state lives across the fork.
( i=0; while true; do i=$((i+1)); echo $i > /tmp/ticks; sleep 0.1; done ) &
echo "drawn before the fork: $(head -c 6 /dev/urandom | od -An -tx1 | tr -d ' ')" > /tmp/note
sleep 1

role=$(/bin/forkme)
echo "[role $role] resumed: ticker at $(cat /tmp/ticks), $(cat /tmp/note)"
echo "[role $role] random after fork: $(head -c 6 /dev/urandom | od -An -tx1 | tr -d ' ')"
echo "$role" > /tmp/whoami
sleep 1
echo "[role $role] 1 s later: ticker at $(cat /tmp/ticks), /tmp/whoami=$(cat /tmp/whoami), uptime $(cut -d' ' -f1 /proc/uptime) s"
poweroff -f
