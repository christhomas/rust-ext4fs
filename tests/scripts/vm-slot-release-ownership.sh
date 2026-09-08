#!/usr/bin/env bash
#
# vm-slot-release-ownership.sh — who may free the oracle slot.
#
# `cmd_release` is called unconditionally by `vm.sh down`, including
# from a repository that never booted anything, so "release something I
# do not hold" is an ordinary event rather than an exotic one. The whole
# of the slot's value rests on that call being a no-op.
#
# THE EXIT CODE CARRIES NO SIGNAL. `release` returns 0 whether it freed
# the lock or declined to, which is deliberate — a `down` that says
# nothing is the point — and it means a test that checks the exit code
# passes against a release that frees everything. So every assertion
# here is on whether the lock directory is still there afterwards.
#
# The third case exists so that an over-correction fails. "Never release
# anything" closes the hole in the second case and would look like extra
# safety; it also strands the slot for ever, and only a holder releasing
# its own slot notices.
#
#   bash tests/scripts/vm-slot-release-ownership.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT

export AM_ORACLE_VM_STATE="$sandbox/state"
LOCK="$AM_ORACLE_VM_STATE/slot.lock"

# The directory this repository's own release call claims to hold, which
# is how `vm-slot.sh` identifies itself.
SELF="$REPO/tests/vagrant"
OTHER="$sandbox/some/other/repo/tests/vagrant"

release() { "$REPO/scripts/vm-slot.sh" release >/dev/null 2>&1; }

# Lay down a lock; with a holder file when one is given, without when
# not — the second being the ordinary state of `cmd_acquire` between its
# `mkdir` and the `printf` that records who took it.
set_lock() {
    rm -rf "$LOCK"
    mkdir -p "$LOCK"
    if [ "$#" -gt 0 ]; then
        printf '%s\t%s\t%s\n' "$1" "holder-repo" "$(date +%s)" > "$LOCK/holder"
    fi
}

check_lock() {
    local want="$1" what="$2" got
    if [ -d "$LOCK" ]; then got=survived; else got=removed; fi
    if [ "$got" = "$want" ]; then
        printf 'ok    %s\n' "$what"
    else
        printf 'FAIL  %s: lock %s, expected %s\n' "$what" "$got" "$want"
        fails=$((fails + 1))
    fi
}

# 1. Somebody else holds it, and says so. The long-standing case, and
#    the one that already worked.
set_lock "$OTHER"
release
check_lock survived "a recorded holder's slot is not freed by anyone else"

# 2. The lock exists and nobody has recorded a holder yet. This is not a
#    corrupt state: it is `cmd_acquire` mid-way through taking the slot.
#    Freeing it here hands the same slot to a second caller, and both
#    then boot a 4 GB machine.
set_lock
release
check_lock survived "a lock with no holder recorded yet is not freed by a non-holder"

# 3. The holder releasing its own slot must still work, or the slot is
#    stranded for ever and the fix for 2 is worse than the defect.
set_lock "$SELF"
release
check_lock removed "the holder can still release its own slot"

# 4. `--force` is the documented escape hatch for a slot nobody will
#    give back, and 2 must not have disarmed it.
set_lock
"$REPO/scripts/vm-slot.sh" release --force >/dev/null 2>&1
check_lock removed "--force still frees a lock with no holder recorded"

# 5. Declining in case 2 must not strand the lock, because `acquire`
#    reclaims that state itself: it waits a moment for a holder mid-write
#    and then breaks the lock. Without this the fix for 2 would trade a
#    race for a deadlock.
set_lock
if AM_ORACLE_VM_WAIT=20 "$REPO/scripts/vm-slot.sh" acquire >/dev/null 2>&1; then
    printf 'ok    acquire still reclaims a lock with no holder recorded\n'
else
    printf 'FAIL  acquire could not reclaim an unheld lock: the slot is stranded\n'
    fails=$((fails + 1))
fi

# 6. A HOLDER MID-WRITE MUST NOT BE ROBBED.
#
#    `cmd_acquire` takes the slot in two steps — `mkdir`, then write the
#    holder record — so between them the lock exists with an EMPTY
#    holder file. The `sleep 1` in the wait loop exists to let that
#    write land, and it is reached only when `read_holder` reports that
#    there is no record yet.
#
#    `read_holder` used to succeed on an empty file, so the grace never
#    fired: the waiter read an empty timestamp, computed an age of 56
#    years, found field 1 empty and therefore "dead", and broke the lock
#    of a process that was one statement from finishing.
#
#    This asserts the grace by using it: lay down an empty holder file,
#    start a waiter, and complete the record while the waiter is inside
#    its grace. The lock must still belong to the original holder.
#    The assertion is on whether the WAITER ACQUIRED, not on the holder
#    file's contents: the file is at a fixed path, so a waiter that
#    breaks the lock and a holder that completes its write both end up
#    writing it, and comparing its contents cannot tell them apart. The
#    exit status of `acquire` can — it is 0 only if the waiter took the
#    slot. (`release`'s exit status carries no signal, as the header
#    says; `acquire`'s is the whole answer, which is why case 5 uses it
#    too.)
set_lock
: > "$LOCK/holder"                   # the mid-write state, exactly
(
    sleep 0.3
    printf '%s\t%s\t%s\n' "$OTHER" "holder-repo" "$(date +%s)" > "$LOCK/holder"
) &
completer=$!
if AM_ORACLE_VM_WAIT=6 "$REPO/scripts/vm-slot.sh" acquire >/dev/null 2>&1; then
    printf 'FAIL  a waiter took the slot from a holder that was mid-write\n'
    fails=$((fails + 1))
else
    printf 'ok    a holder that completes its record within the grace keeps the slot\n'
fi
wait "$completer" 2>/dev/null || true

# 7. And the grace must not become a deadlock: a holder that never
#    finishes its write is still reclaimed, which is case 5 with an
#    empty file rather than a missing one. Without this, "require a
#    well-formed record" could be satisfied by waiting for ever.
set_lock
: > "$LOCK/holder"
if AM_ORACLE_VM_WAIT=20 "$REPO/scripts/vm-slot.sh" acquire >/dev/null 2>&1; then
    printf 'ok    acquire still reclaims a lock whose holder record never arrived\n'
else
    printf 'FAIL  acquire could not reclaim a lock with an empty holder file\n'
    fails=$((fails + 1))
fi

# 8. A LIVE VM'S SLOT SURVIVES, HOWEVER LONG IT HAS BEEN HELD.
#
#    `cmd_acquire` used to break any lock older than 90 minutes without
#    asking whether a VM was still running. A fixture build routinely
#    outlives that — `vm.sh up` and `vm-e2fsck.sh` deliberately leave
#    the machine up between invocations — so the waiter took the slot
#    from a live VM and booted a second 4 GB one beside it.
#
#    `holder_is_dead` decides liveness by looking for a qemu process
#    whose command line names the holder's directory, so a stand-in
#    process with that shape is enough to make the holder live. The
#    holder's timestamp is set a day in the past, far beyond any
#    timeout that ever existed here.
set_lock
printf '%s\t%s\t%s\n' "$OTHER" "holder-repo" "$(( $(date +%s) - 86400 ))" > "$LOCK/holder"
( exec -a "qemu-system-stand-in -drive file=$OTHER/disk.img" sleep 30 ) &
faux_vm=$!
sleep 0.3
if AM_ORACLE_VM_WAIT=6 "$REPO/scripts/vm-slot.sh" acquire >/dev/null 2>&1; then
    printf 'FAIL  a waiter took the slot from a VM that is still running\n'
    fails=$((fails + 1))
else
    printf 'ok    a live VM keeps its slot however long it has held it\n'
fi
kill "$faux_vm" 2>/dev/null || true
wait "$faux_vm" 2>/dev/null || true

# 9. And the same lock with no VM behind it is still reclaimed, so the
#    fix for 8 is "require liveness", not "never break anything".
set_lock
printf '%s\t%s\t%s\n' "$OTHER" "holder-repo" "$(( $(date +%s) - 86400 ))" > "$LOCK/holder"
if AM_ORACLE_VM_WAIT=20 "$REPO/scripts/vm-slot.sh" acquire >/dev/null 2>&1; then
    printf 'ok    a lock whose VM is gone is still reclaimed\n'
else
    printf 'FAIL  a dead holder lock was not reclaimed: the slot is stranded\n'
    fails=$((fails + 1))
fi

# 10. A LINE THAT IS NOT A RECORD MUST NOT READ AS ONE.
#
#     `cut -f` returns the whole line for every field index when the
#     delimiter is absent, so `read_holder` parsed a holder file reading
#     `123` as dir=123, repo=123, since=123 — a valid record naming a
#     directory called `123`, dated 1970. The grace that case 6 asserts
#     was skipped for exactly the same reason as before it was fixed,
#     just through a different door.
#
#     Asserted the same way as case 6, because the observable
#     consequence is the same one: a waiter must not take the slot from
#     a holder that completes its record within the grace.
# The four-field case moved out of this list when the generation token
# arrived: four is now a record this script writes, and the upward bound
# is five.
for bad in '123' '/d\tr\t1\tt\textra'; do
    set_lock
    printf "$bad\n" > "$LOCK/holder"
    (
        sleep 0.3
        printf '%s\t%s\t%s\n' "$OTHER" "holder-repo" "$(date +%s)" > "$LOCK/holder"
    ) &
    completer=$!
    if AM_ORACLE_VM_WAIT=6 "$REPO/scripts/vm-slot.sh" acquire >/dev/null 2>&1; then
        printf 'FAIL  a waiter took the slot after reading %s as a record\n' "$bad"
        fails=$((fails + 1))
    else
        printf 'ok    a holder file of %s is not mistaken for a record\n' "$bad"
    fi
    wait "$completer" 2>/dev/null || true
done

# 11. And a well-formed record is still read, so 10 cannot be satisfied
#     by rejecting everything — which would strand the slot for ever.
set_lock "$OTHER"
release
check_lock survived "a well-formed record is still read as one"

if [ "$fails" -eq 0 ]; then
    echo "PASS  vm slot release ownership"
else
    echo "FAIL  $fails assertion(s)" >&2
    exit 1
fi
