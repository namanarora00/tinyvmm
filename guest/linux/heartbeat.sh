#!/bin/sh
# Runs as PID 1 when the kernel command line has tinyvmm.demo=heartbeat.
# Writes '@' to the console nonstop. After a fork or snapshot restore, the
# first '@' that comes out marks "the clone is running guest code again",
# measured the same way for tinyvmm and Firecracker.
echo "heartbeat: start"
while :; do echo -n @; done
