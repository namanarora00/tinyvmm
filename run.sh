#!/bin/sh
# Build (release) and run tinyvmm inside the Colima VM: the only place with /dev/kvm.
# Colima mounts the Mac home folder at the same path, so the source is shared.
# The target dir lives on the VM's own disk because building onto the shared mount is slow.
# Usage: ./run.sh [tinyvmm args...]
set -e
DIR=$(cd "$(dirname "$0")" && pwd)
exec colima ssh -- sh -c "cd '$DIR' && CARGO_TARGET_DIR=\$HOME/tinyvmm-target ~/.cargo/bin/cargo build --release -q && sg kvm -c '\$HOME/tinyvmm-target/release/tinyvmm $*'"
