#!/usr/bin/env bash
#
# vm-e2fsck.sh <image> [<image>...] — run `e2fsck -fn` on each image
# inside the Debian arm64 oracle VM.
#
# This is the real-Linux-ext4 oracle for driver-mutated images: no host
# e2fsprogs, no Docker, and no marking our own homework. A driver that
# only checks its own work against its own reader proves consistency,
# not correctness.
#
# The interface is unchanged from the version that drove an emulated
# x86_64 Alpine guest — same arguments, same exit semantics — so callers
# port across untouched. What changed underneath is the guest: an arm64
# Debian VM under QEMU/HVF runs at hardware speed, where the x86_64
# guest meant full CPU emulation on Apple Silicon for every check.
#
# Exit status is non-zero if e2fsck reported a problem with any image.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHARE="$REPO/.vm-share"
VM="$REPO/scripts/vm.sh"

[ $# -gt 0 ] || { echo "usage: vm-e2fsck.sh <image> [<image>...]" >&2; exit 2; }

"$VM" up
mkdir -p "$SHARE"

# Stage copies rather than the originals: e2fsck is run with -n so it
# does not write, but a fixture is not worth risking to prove that.
staged=()
for img in "$@"; do
    [ -f "$img" ] || { echo "no such image: $img" >&2; exit 2; }
    base="$(basename "$img")"
    cp "$img" "$SHARE/$base"
    staged+=("$base")
done

rc=0
for base in "${staged[@]}"; do
    echo "############ e2fsck $base ############"
    # `-f` forces a full check even when the superblock says clean, and
    # `-n` answers no to every repair prompt, so this reports without
    # touching the image.
    if ! "$VM" run "e2fsck -fn /share/$base"; then
        rc=1
    fi
    echo
done

for base in "${staged[@]}"; do
    rm -f "$SHARE/$base"
done

if [ "$rc" -ne 0 ]; then
    echo "e2fsck reported problems — see the output above." >&2
fi
exit "$rc"
