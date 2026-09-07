#!/usr/bin/env bash
#
# vm-slot.sh — one oracle VM at a time, across every repository.
#
#   vm-slot.sh acquire     wait for the slot, then take it
#   vm-slot.sh release     give it back (idempotent)
#   vm-slot.sh status      say who holds it and for how long
#
# WHY THIS EXISTS. These VMs are not small: this one asks for 4 GB and
# the NTFS Windows box asks for 8. Two of them plus a `cargo test` fills
# a laptop, and when it fills, the machine does not fail cleanly -- it
# starts killing background work. That is exactly what happened on the
# night several agents were building fixtures in parallel: two VMs left
# running for hours, and unrelated jobs killed for want of memory with
# no message that connected the two.
#
# So the slot is deliberately a single global one rather than one per
# repository. The cost is real and worth stating: fixture builds in
# different repositories no longer overlap, and a queued build waits for
# the one ahead of it. That is slower on a good day and much better on a
# bad one, because the failure it removes was silent and the cost it
# adds is visible.
#
# The state lives outside every repository, because the whole point is
# that repositories do not know about each other.
set -euo pipefail

STATE_DIR="${AM_ORACLE_VM_STATE:-$HOME/.local/state/am-oracle-vm}"
LOCK="$STATE_DIR/slot.lock"
HOLDER="$LOCK/holder"

# How long to wait before giving up, and how long a holder may keep the
# slot before another waiter is allowed to take it away.
#
# The wait is long because a fixture build legitimately takes tens of
# minutes; the breaking point is longer still, because breaking a lock
# someone is genuinely using is worse than waiting. Both are overridable
# for a caller that knows better.
WAIT_SECS="${AM_ORACLE_VM_WAIT:-3600}"
STALE_SECS="${AM_ORACLE_VM_STALE:-5400}"

# How long a freshly taken slot is trusted before the "is a VM actually
# running" test is allowed to break it.
#
# THE SLOT IS TAKEN BEFORE THE VM EXISTS -- necessarily, since the point
# is to stop a second one booting. So for the length of a `vagrant up`
# there is a holder with no VM behind it, and without this a waiter
# would look, see nothing running, break the lock, and boot the second
# VM this whole file exists to prevent. Both would then be up and each
# would think it held the slot.
#
# Three minutes covers a cold boot with provisioning on this hardware.
# Too long only delays reclaiming a genuinely dead lock; too short
# reintroduces the race, so it errs long.
BOOT_GRACE_SECS="${AM_ORACLE_VM_BOOT_GRACE:-180}"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_NAME="$(basename "$REPO")"
# The directory whose VM this slot is being held for. It is what makes
# staleness answerable: a running qemu names its disk image, and that
# path is under here.
VAGRANT_DIR="$REPO/tests/vagrant"

now() { date +%s; }

# The holder record, or empty if the slot is free.
#   vagrant_dir<TAB>repo<TAB>epoch
#
# NOT A PID. `acquire` is its own short-lived process -- it takes the
# slot and exits, leaving the VM behind -- so a PID recorded here is
# dead within milliseconds and every lock would look stale immediately.
# The first version of this file did exactly that, and its own status
# command reported "that process is gone" one line after acquiring.
#
# What actually holds the slot is a RUNNING VM, so that is what is
# recorded and what staleness is measured against.
read_holder() {
    [ -f "$HOLDER" ] || return 1
    cat "$HOLDER" 2>/dev/null
}

holder_field() { read_holder | cut -f"$1"; }

pretty_age() {
    local secs=$1
    if [ "$secs" -lt 60 ]; then echo "${secs}s"
    elif [ "$secs" -lt 3600 ]; then echo "$((secs / 60))m"
    else echo "$((secs / 3600))h$(((secs % 3600) / 60))m"
    fi
}

# A lock is stale when no VM is actually running for the directory that
# took it. A qemu process names its disk image on the command line, and
# that image lives under the holder's vagrant directory, so one `ps` is
# enough and no cooperation from the holder is needed.
#
# This is the check that survives a crash: a script killed between
# booting a VM and halting it leaves both the lock and the VM, and the
# VM is the thing worth noticing. A script killed before the VM came up
# leaves a lock nothing is using, and this frees it.
holder_is_dead() {
    local dir procs
    dir="$(holder_field 1 2>/dev/null || true)"
    [ -n "$dir" ] || return 0

    # NO `grep -q` ON THE END OF THIS PIPELINE, and the reason is worth
    # keeping. `set -o pipefail` is on at the top of this file. `grep -q`
    # exits the instant it matches, which closes the pipe, which sends
    # `ps` a SIGPIPE, which makes the PIPELINE exit 141 -- *because* the
    # match succeeded. Negated, that reads as "no VM is running", so the
    # check reported the opposite of the truth precisely when it found
    # what it was looking for, and every held slot looked stale.
    #
    # Caught by running it against a VM that was demonstrably up: `ps`
    # by hand found the process, and this function said it had not.
    #
    # A grep without `-q` reads its input to the end, so nothing gets a
    # SIGPIPE, and the match is then tested against a plain string.
    procs="$(ps -eo args 2>/dev/null | grep qemu || true)"
    case "$procs" in
        *"$dir"*) return 1 ;;   # a VM is running for it: not dead
    esac
    return 0
}

break_lock() {
    local why="$1"
    echo "[vm-slot] breaking the lock: $why" >&2
    rm -rf "$LOCK"
}

cmd_acquire() {
    mkdir -p "$STATE_DIR"
    local waited=0 announced=0

    while :; do
        # `mkdir` is the atomic step. Two processes racing here, one wins
        # and the other loops -- which is the whole reason the lock is a
        # directory rather than a file somebody has to check-then-write.
        if mkdir "$LOCK" 2>/dev/null; then
            printf '%s\t%s\t%s\n' "$VAGRANT_DIR" "$REPO_NAME" "$(now)" > "$HOLDER"
            [ "$announced" = 1 ] && echo "[vm-slot] got the slot after $(pretty_age $waited)" >&2
            return 0
        fi

        # Somebody holds it. Decide whether they still exist.
        if ! read_holder >/dev/null 2>&1; then
            # The directory exists with no holder file: a process died
            # between the two steps. Give it a moment in case it is
            # simply mid-write, then take it.
            sleep 1
            read_holder >/dev/null 2>&1 || { break_lock "no holder recorded"; continue; }
        fi

        local since age
        since="$(holder_field 3)"
        age=$(( $(now) - ${since:-0} ))

        if [ "$age" -gt "$BOOT_GRACE_SECS" ] && holder_is_dead; then
            break_lock "no VM is running for $(holder_field 2) after $(pretty_age $age)"
            continue
        fi
        if [ "$age" -gt "$STALE_SECS" ]; then
            break_lock "held by $(holder_field 2) for $(pretty_age $age), past the $(pretty_age $STALE_SECS) limit"
            continue
        fi

        if [ "$announced" = 0 ]; then
            echo "[vm-slot] waiting for the oracle slot — held by $(holder_field 2) for $(pretty_age $age)" >&2
            echo "[vm-slot] one VM runs at a time; this is a queue, not a failure" >&2
            announced=1
        fi

        if [ "$waited" -ge "$WAIT_SECS" ]; then
            echo "[vm-slot] gave up after $(pretty_age $waited) waiting for $(holder_field 2)" >&2
            echo "[vm-slot] if that repository is finished, run its 'scripts/vm.sh down'," >&2
            echo "[vm-slot] or 'scripts/vm-slot.sh release --force' to take the slot." >&2
            return 1
        fi

        sleep 5
        waited=$((waited + 5))
    done
}

cmd_release() {
    # Only the holder may release, unless forced. Otherwise a script
    # that never took the slot can free somebody else's, which is the
    # same bug as not having a lock at all.
    if [ "${1:-}" = "--force" ]; then
        rm -rf "$LOCK"
        return 0
    fi
    if ! read_holder >/dev/null 2>&1; then
        # The lock exists with no holder recorded: somebody is between
        # `mkdir` and the write. Not ours to free -- acquire reclaims it.
        return 0
    fi
    if [ "$(holder_field 1)" != "$VAGRANT_DIR" ]; then
        # Somebody else's slot. Leave it alone, and say nothing:
        # `down` calls this unconditionally, and a repository halting
        # a VM it never booted is ordinary rather than an error.
        return 0
    fi
    rm -rf "$LOCK"
}

cmd_status() {
    if ! read_holder >/dev/null 2>&1; then
        echo "the oracle slot is free"
        return 0
    fi
    local age
    age=$(( $(now) - $(holder_field 3) ))
    printf 'held by %s for %s\n' "$(holder_field 2)" "$(pretty_age $age)"
    if holder_is_dead; then
        if [ "$age" -le "$BOOT_GRACE_SECS" ]; then
            echo "  ...no VM yet, but it is within the $(pretty_age $BOOT_GRACE_SECS) boot window"
        else
            echo "  ...but no VM is running for it; the next acquire will take the slot"
        fi
    fi
    return 0
}

case "${1:-}" in
    acquire) cmd_acquire ;;
    release) shift; cmd_release "${1:-}" ;;
    status)  cmd_status ;;
    *)
        echo "usage: vm-slot.sh {acquire|release [--force]|status}" >&2
        exit 2
        ;;
esac
