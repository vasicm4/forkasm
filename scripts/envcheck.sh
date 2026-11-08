#!/usr/bin/env bash
# Checks the kernel features forkasm's memory strategies depend on.
# Paste the output into NOTES.md.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "date:                    $(date -u +%Y-%m-%dT%H:%MZ)"
echo "kernel:                  $(uname -r)"
echo "cpu:                     $(lscpu | sed -n 's/^Model name: *//p') ($(nproc) threads)"
echo "vm.max_map_count:        $(cat /proc/sys/vm/max_map_count)"
echo "unprivileged_userfaultfd: $(cat /proc/sys/vm/unprivileged_userfaultfd 2>/dev/null || echo n/a)"
echo "shmem THP:               $(cat /sys/kernel/mm/transparent_hugepage/shmem_enabled 2>/dev/null || echo n/a)"
echo "rustc:                   $(rustc --version)"
echo "wasmtime:                $(cargo tree -p forkasm -i wasmtime --depth 0 2>/dev/null | head -1)"
echo
cargo run --quiet --release -p forkasm-cli -- envcheck
