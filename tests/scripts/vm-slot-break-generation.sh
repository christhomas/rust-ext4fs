#!/usr/bin/env bash
#
# vm-slot-break-generation.sh — a break deletes the generation it was
# authorised against, or nothing.
#
# `$LOCK` is a fixed path. A waiter decides a holder is stale — reading
# the record, running a `ps`, sometimes sleeping — and then deletes
# whatever is at that path. If another waiter broke and acquired in
# between, what it deletes is a LIVE lock, and both callers go on to
# boot a 4 GB machine.
#
# THE WINDOW CANNOT BE REACHED THROUGH THE CLI. It is a few microseconds
# wide and lives inside one invocation, so this sources the script — with
# `AM_VM_SLOT_LIB` set it defines its functions and stops — and calls the
# real `break_lock` with a token from a generation that has since been
# replaced. Testing a reimplementation of the shape here would assert
# nothing about the script.
#
#   bash tests/scripts/vm-slot-break-generation.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
export AM_ORACLE_VM_STATE="$sandbox/state"
mkdir -p "$AM_ORACLE_VM_STATE"

AM_VM_SLOT_LIB=1
export AM_VM_SLOT_LIB
# shellcheck source=/dev/null
. "$REPO/scripts/vm-slot.sh"
# The script sets `-e` for its own benefit and sourcing brings that here.
# `break_lock` returning non-zero is the RESULT under test, not a
# failure, so restore this file's own options.
set +e

check() {
    local want="$1" what="$2" got
    if [ -d "$LOCK" ]; then got=survived; else got=removed; fi
    if [ "$got" = "$want" ]; then
        printf 'ok    %s\n' "$what"
    else
        printf 'FAIL  %s: lock %s, expected %s\n' "$what" "$got" "$want"
        fails=$((fails + 1))
    fi
}

put_generation() {
    rm -rf "$LOCK"
    mkdir -p "$LOCK"
    printf '%s\t%s\t%s\t%s\n' "/some/other/repo" "holder-repo" "$(date +%s)" "$1" > "$HOLDER"
}

# 1. The race itself. Take generation one's token, let the lock be
#    replaced by generation two — which is what another waiter breaking
#    and acquiring looks like — and present the stale token. The
#    replacement must survive.
put_generation "generation-one"
stale="$(holder_field 4)"
put_generation "generation-two"
break_lock "a decision about generation one" "$stale" >/dev/null 2>&1
check survived "a break authorised against an earlier generation spares its replacement"

# 2. The control, without which "never break anything" would pass 1.
#    A break presented with the CURRENT generation's token must work, or
#    a stale lock is never reclaimed and the slot strands.
put_generation "generation-three"
current="$(holder_field 4)"
break_lock "a decision about generation three" "$current" >/dev/null 2>&1
check removed "a break authorised against the current generation still frees it"

# 3. A lock written before tokens existed has an empty fourth field. The
#    caller reads the same empty value, so it still matches and such a
#    lock is still reclaimable — the fix must not strand one.
rm -rf "$LOCK"; mkdir -p "$LOCK"
printf '%s\t%s\t%s\n' "/some/other/repo" "holder-repo" "$(date +%s)" > "$HOLDER"
break_lock "a lock with no token" "$(holder_field 4)" >/dev/null 2>&1
check removed "a tokenless lock from an older version is still reclaimable"

# 3b. THE RELEASE PATH HAS THE SAME BINDING AND IT IS THE ONE THAT RUNS.
#
#     `vm.sh down` calls `cmd_release` on every teardown, so this window
#     is reached far more often than a break is — and it is most
#     reachable at exactly the moment the function runs, because teardown
#     is when this repository's VM has just stopped. A waiter sampling
#     `holder_is_dead` right then breaks and acquires legitimately, and a
#     release already past its `holder_field 4` read moves and deletes
#     THEIR lock.
#
#     Separating the two halves needs the read to answer with one
#     generation while the disk holds another, which is what the
#     `holder_field` override does. Field 1 still names this repository,
#     so the ownership check ahead of the binding passes and the binding
#     is what is under test.
release_with_stale_token() {
    local stale="$1"
    holder_field() {
        case "$1" in
            1) printf '%s\n' "$VAGRANT_DIR" ;;
            4) printf '%s\n' "$stale" ;;
            *) printf 'holder-repo\n' ;;
        esac
    }
    cmd_release
    unset -f holder_field
}

rm -rf "$LOCK"; mkdir -p "$LOCK"
printf '%s\t%s\t%s\t%s\n' "$VAGRANT_DIR" "holder-repo" "$(date +%s)" "generation-four" > "$HOLDER"
release_with_stale_token "generation-three"
check survived "a release authorised against an earlier generation spares its replacement"

# 3c. And the mirror, or `vm.sh down` strands the slot on every
#     teardown: a release whose token matches must still free the lock it
#     genuinely holds.
rm -rf "$LOCK"; mkdir -p "$LOCK"
printf '%s\t%s\t%s\t%s\n' "$VAGRANT_DIR" "holder-repo" "$(date +%s)" "generation-five" > "$HOLDER"
release_with_stale_token "generation-five"
check removed "a release holding the current generation still frees its slot"

# 4. Nothing is left behind. The deletion goes through a rename into a
#    private path; if that path survived, a later run would trip over it.
if [ -z "$(find "$AM_ORACLE_VM_STATE" -maxdepth 1 -name 'slot.lock.*' -print -quit 2>/dev/null)" ]; then
    printf 'ok    no staging directory is left behind\n'
else
    printf 'FAIL  a staging directory survived: %s\n' \
        "$(find "$AM_ORACLE_VM_STATE" -maxdepth 1 -name 'slot.lock.*' -print -quit)"
    fails=$((fails + 1))
fi

if [ "$fails" -eq 0 ]; then
    echo "PASS  vm slot break generation"
else
    echo "FAIL  $fails assertion(s)" >&2
    exit 1
fi
