#!/usr/bin/env bash
#
# vm.sh — drive the Debian arm64 oracle VM.
#
#   vm.sh up            boot the VM (idempotent; provisions on first run)
#   vm.sh run <cmd...>  run a command inside the VM
#   vm.sh share         print the host path of the shared directory
#   vm.sh put <file>    copy a file into the shared directory, echo guest path
#   vm.sh down          halt the VM (state is kept; next `up` is fast)
#   vm.sh destroy       delete the VM entirely
#
# The VM is the real-Linux oracle: mkfs.xfs, the in-kernel XFS driver and
# xfs_repair are Linux-only, and validating this driver against anything
# less than a real kernel would just be marking our own homework.
#
# The VM is kept running between invocations on purpose. Booting is the
# slow part; an iterate-and-check loop should pay it once.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VAGRANT_DIR="$REPO/tests/vagrant/debian"
SHARE="$REPO/.vm-share"
# One oracle VM runs at a time, across every repository — see
# scripts/vm-slot.sh for why. Absent (an older checkout, a partial
# copy), everything below still works and the serialisation is simply
# not enforced; a missing helper should not stop a developer building
# fixtures.
SLOT="$REPO/scripts/vm-slot.sh"

mkdir -p "$SHARE"

vm_up() {
    # `vagrant status` is authoritative but slow-ish; only boot when the
    # machine is not already running.
    if ! (cd "$VAGRANT_DIR" && vagrant status --machine-readable 2>/dev/null \
            | grep -q ',state,running'); then
        # TAKE THE SLOT BEFORE BOOTING, and only when actually booting.
        # A machine already up took the slot when it started, so asking
        # again here would deadlock a second `up` against itself.
        if [ -x "$SLOT" ]; then
            "$SLOT" acquire || {
                echo "vm: could not get the oracle slot; not booting a second VM." >&2
                exit 1
            }
        fi
        if ! (cd "$VAGRANT_DIR" && vagrant up); then
            # A boot that failed is not holding a VM, and keeping the
            # slot would make every other repository wait for a machine
            # that will never exist.
            [ -x "$SLOT" ] && "$SLOT" release || true
            exit 1
        fi
    fi
}

case "${1:-}" in
    up)
        vm_up
        ;;
    run)
        shift
        vm_up
        # `vagrant ssh -c` mangles quoting for complex commands; feed the
        # command on stdin instead so the guest shell sees it verbatim.
        printf '%s\n' "$*" | (cd "$VAGRANT_DIR" && vagrant ssh -- -T 'sudo bash -s')
        ;;
    share)
        echo "$SHARE"
        ;;
    put)
        [ $# -eq 2 ] || { echo "usage: vm.sh put <file>" >&2; exit 2; }
        cp "$2" "$SHARE/"
        echo "/share/$(basename "$2")"
        ;;
    down)
        (cd "$VAGRANT_DIR" && vagrant halt)
        # CONFIRM BEFORE RELEASING. `vagrant halt` reporting success is
        # not the same as the machine being down, and a slot handed back
        # while the VM still runs lets the next repository boot a second
        # one beside it — the exact thing this serialisation exists to
        # prevent.
        state=$(cd "$VAGRANT_DIR" && vagrant status --machine-readable 2>/dev/null \
                | sed -n 's/.*,state,//p' | head -1)
        case "$state" in
            running)
                echo "vm: halt did not stop the machine — it is still running." >&2
                echo "    The oracle slot is deliberately kept; \`vm.sh destroy\` reclaims it." >&2
                exit 1
                ;;
            *)
                [ -x "$SLOT" ] && "$SLOT" release || true
                ;;
        esac
        ;;
    destroy)
        (cd "$VAGRANT_DIR" && vagrant destroy -f)
        # `-f` leaves nothing running whether or not it complained, so
        # the slot goes back unconditionally.
        [ -x "$SLOT" ] && "$SLOT" release || true
        ;;
    *)
        sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 2
        ;;
esac
