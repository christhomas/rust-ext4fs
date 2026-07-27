#!/usr/bin/env bash
# guard: github-protect-main
# Ensure the repo's default branch is protected: require a PR (no direct
# pushes), enforced for admins too, linear history, no force-push/deletion —
# i.e. force everyone into PR mode. Owner-only, fail-open — NEVER blocks.
set -u
dir=$(cd "$(dirname "$0")/.." && pwd)
# shellcheck source=../lib/common.sh
. "$dir/lib/common.sh"

slug=$(gg_repo_slug); [ -n "$slug" ] || exit 0
gg_have_gh || { echo "github-guard: gh not installed/authed — skipping branch-protection check for $slug" >&2; exit 0; }
owner=${slug%%/*}
gg_user_owns "$owner" || exit 0

branch=$(gh api "repos/$slug" --jq '.default_branch' 2>/dev/null) || {
  echo "github-guard: couldn't read default branch for $slug — skipping" >&2; exit 0; }
[ -n "$branch" ] || exit 0

# Required status checks: auto-discover the checks that GATE A PULL REQUEST and
# require them, strict. The only checks that can gate a PR are the ones that run
# on `pull_request`, so discover them from recent pull_request workflow runs —
# NOT from the default branch's HEAD commit. That commit also carries release
# checks triggered by a tag push OR a `workflow_dispatch` on the branch; those
# never run on a PR, and requiring them makes every PR wait forever on checks
# that can't complete. Take the latest pull_request run per workflow and union
# their check-run names: exactly the PR gate. The github-actions app filter
# still excludes third-party checks like coderabbit. Self-healing — a renamed or
# added CI job syncs after the next PR runs it AND it has passed on main once
# (see the `passed` gate below); never strips existing checks on a transient
# empty discovery. jq required; without it we preserve whatever's already set
# (fail-open).
#
# Both `desired` (here) and `current` (the read-back below) end as compact JSON
# straight from jq (`jq -c` / `tojson`), so escaping (quotes, backslashes) and
# sort order match and the equality check below is exact.
desired='[]'
if command -v jq >/dev/null 2>&1; then
  # check-suite of the latest pull_request run of each PR-triggering workflow.
  # per_page=100 (not 50) widens the window so a workflow whose latest PR run is
  # a bit older still gets discovered; the union below is the backstop for gaps.
  suite_ids=$(gh api "repos/$slug/actions/runs?event=pull_request&per_page=100" \
    --jq '[.workflow_runs[]?] | group_by(.workflow_id)[] | max_by(.created_at) | .check_suite_id' 2>/dev/null)
  desired=$(
    for sid in $suite_ids; do
      gh api --paginate "repos/$slug/check-suites/$sid/check-runs?per_page=100" \
        --jq '.check_runs[] | select(.app.slug=="github-actions") | .name' 2>/dev/null
    done | jq -sRc 'split("\n") | map(select(length > 0)) | map({context: .}) | unique'
  )
  # Empty stdin yields "[]" here, but guard against a stray "null" too so a
  # malformed value can never reach the protection PUT payload.
  case "$desired" in '' | null) desired='[]' ;; esac
fi

# Checks that have ACTUALLY PASSED on the default branch — the gate for ADDING a
# newly-discovered check to REQUIRED. A check discovered from PR runs is only
# eligible to become required once it has concluded `success` on the default
# branch's latest commit at least once; a check that has only ever been
# pending/failure is NOT added. Without this gate the additive union below would
# auto-require a brand-new or still-red check (e.g. a just-landed "E2E (full
# topology)" that has never gone green on main), and because we also set
# enforce_admins + strict that check would block EVERY merge — no one, not even
# an admin, could merge until a check that has never passed somehow passes. We
# never STRIP already-required checks (the union preserves `current`), so this
# gates only the additive step. Query the default branch HEAD's check-runs and
# keep the github-actions ones that concluded `success`. jq required; without it
# `passed` stays '[]' so nothing new is added and `current` is preserved
# (fail-open — never blocks). Empty/failed query behaves the same: add nothing.
passed='[]'
if command -v jq >/dev/null 2>&1; then
  passed=$(gh api --paginate "repos/$slug/commits/$branch/check-runs?per_page=100" \
    --jq '.check_runs[]? | select(.app.slug=="github-actions") | select(.conclusion=="success") | .name' 2>/dev/null \
    | jq -sRc 'split("\n") | map(select(length > 0)) | unique')
  case "$passed" in '' | null) passed='[]' ;; esac
fi

# ALL github-actions check-run NAMES on the default-branch HEAD (ANY conclusion)
# — the evidence that lets the matrix-parent prune below fire ONLY on positive
# proof a required context has vanished. Unlike `passed` (success-only, the
# promotion gate) this keeps pending/failed runs too, because the prune's only
# question is "does a check by this EXACT name still RUN on main at all". A real,
# independent required check whose latest PR run merely fell outside the bounded
# discovery window above still runs here, so it is never mistaken for a stale
# matrix parent and stripped. jq required; empty on failure, and the prune treats
# an empty list as "no evidence" and preserves the context (fail-open).
head_names='[]'
if command -v jq >/dev/null 2>&1; then
  head_names=$(gh api --paginate "repos/$slug/commits/$branch/check-runs?per_page=100" \
    --jq '.check_runs[]? | select(.app.slug=="github-actions") | .name' 2>/dev/null \
    | jq -sRc 'split("\n") | map(select(length > 0)) | unique')
  case "$head_names" in '' | null) head_names='[]' ;; esac
fi

# Current protection facts in one call: PR reviews present? admins enforced?
# plus the currently-required checks from the modern `checks` field (normalized
# to {context}, sorted). Each value is emitted on its OWN line, NOT through
# `@tsv` — `@tsv` adds a second escaping pass on top of `tojson`, so a job name
# containing `"` or `\` would read back double-escaped and never equal the
# `jq -c`-encoded `desired`, re-applying protection on every commit. `tojson`
# output is single-line, so line-reading each field is safe. Empty when unprotected.
{ IFS= read -r has_reviews; IFS= read -r has_admins; IFS= read -r current; IFS= read -r review_count; } < <(
  gh api "repos/$slug/branches/$branch/protection" --jq \
    '(.required_pull_request_reviews != null),
     (.enforce_admins.enabled // false),
     ((.required_status_checks.checks // []) | map({context: .context}) | unique | tojson),
     (.required_pull_request_reviews.required_approving_review_count // 0)' 2>/dev/null)
[ -n "$current" ] || current='[]'
# Preserve any already-configured approval count instead of hardcoding 0: re-applying
# protection must never silently DOWNGRADE a team's "require N reviews" back to 0.
# Default 0 (require a PR, no approvals) for the solo case / a fresh unprotected branch.
# Sanitize to digits so only a number can reach the JSON payload below.
case "$review_count" in '' | *[!0-9]*) review_count=0 ;; esac

# An OPTIONAL committed declaration OVERRIDES discovery: `.githooks/required-checks`,
# one check-run name per line (`#` comments and blank lines ignored; the single
# word `none` means require no checks at all).
#
# Discovery below requires checks BY JOB NAME, which is only ever a guess at the
# PR gate — and one class of guess is unfixable from the API alone. A workflow
# whose `on: pull_request:` carries `paths:` filters reports NO check run for a PR
# that touches nothing it watches; GitHub reads a required-but-absent check as
# pending, so with enforce_admins nobody can merge and there is no failure to
# point at. (A job skipped by a job-level `if:` is fine — it still reports, with
# conclusion `skipped`, which SATISFIES protection. Only the workflow never
# starting is fatal.) Telling those apart means parsing every workflow's YAML and
# guessing at its filters, so instead a repo can just say what its gate is.
#
# The idiomatic answer is one always-run aggregate job that `needs:` the others,
# named here — then jobs can be renamed, split into a matrix, made conditional or
# path-filtered without ever stranding a merge. Declaring it is also the only way
# to REMOVE a required check that discovery would otherwise keep re-adding, so
# unlike discovery this path is allowed to strip.
declared='[]'
if [ -f "$dir/required-checks" ] && command -v jq >/dev/null 2>&1; then
  declared=$(sed -e 's/#.*//' -e 's/[[:space:]]*$//' -e 's/^[[:space:]]*//' "$dir/required-checks" \
    | jq -sRc 'split("\n") | map(select(length > 0)) | map({context: .}) | unique')
  case "$declared" in '' | null) declared='[]' ;; esac
  if [ "$declared" = '[]' ]; then
    # Comments-only or empty: NOT read as "require nothing" — a file someone
    # blanked mid-edit must not silently unprotect the branch. Fall through to
    # discovery; `none` is the explicit way to ask for an empty set.
    echo "github-guard: $dir/required-checks lists no checks — ignoring it (write 'none' to require none)" >&2
  fi
fi

if [ "$declared" = '[{"context":"none"}]' ]; then
  want='[]'
elif [ "$declared" != '[]' ]; then
  # Declared wins, exactly — except for a context that has NEITHER passed on the
  # default branch NOR is already required. That is the typo gate: a misspelled
  # name here would otherwise be required immediately, never report, and lock the
  # repo with enforce_admins on (the same trap the `passed` promotion gate below
  # exists to avoid). A declared check that is not eligible yet is skipped and
  # self-heals as soon as it goes green on main once.
  want=$(printf '%s\n%s\n%s' "$declared" "$passed" "$current" | jq -sc '
    .[0] as $declared | .[1] as $passed | (.[2] | map(.context)) as $cur |
    $declared | map(select(
      (.context as $c | ($passed | index($c)) != null)
      or (.context as $c | ($cur | index($c)) != null)
    )) | unique')
  skipped=$(printf '%s\n%s' "$declared" "$want" | jq -sc '(.[0] - .[1]) | map(.context)')
  [ "$skipped" = '[]' ] || \
    echo "github-guard: declared checks not required yet (never green on $branch): $skipped" >&2
  if [ "$want" = '[]' ]; then
    # Nothing declared is eligible — a typo, or an aggregate job that has not yet
    # run green on main. Keep whatever is required today rather than stripping the
    # gate down to nothing on the strength of a name that may not exist. It flips
    # to the declared set on the first commit after that check passes on main.
    echo "github-guard: no declared check is eligible yet — keeping current required checks" >&2
    want="$current"
  fi
# Checks to require: be strictly ADDITIVE — union what's already required with the
# newly-discovered checks THAT HAVE PASSED ON MAIN, never a bare replace. A
# discovery that's non-empty but PARTIAL (a PR-gating workflow whose latest run
# fell outside the window above) would otherwise overwrite `current` and silently
# drop the missing workflows' checks — the exact "never strip existing checks"
# promise, broken. The union keeps every already-required check (so nothing is
# ever stripped) and adds a newly-seen check only when it appears in `passed`
# (green on the default branch at least once) — a still-red / never-passed check
# is discovered but NOT promoted to required, so it can't block merges. Empty
# discovery → keep current untouched. (jq required for the union; without it we
# already fell through with desired='[]' and keep current.) Reached only when the
# repo has NOT declared its gate in .githooks/required-checks above.
elif [ -n "$desired" ] && [ "$desired" != "[]" ]; then
  # $desired is only ever non-'[]' when the jq-guarded discovery above populated
  # it, so jq is guaranteed here — union existing (`current`, always kept) with
  # the subset of discovered checks that are also in `passed`. `current` is added
  # unconditionally so this never strips; discovered checks are gated on `passed`
  # so a never-green check is never newly required.
  want=$(printf '%s\n%s\n%s' "$current" "$desired" "$passed" \
    | jq -sc '
        .[0] as $current | .[1] as $desired | .[2] as $passed |
        ( $current
          + ( $desired | map(select(.context as $c | ($passed | index($c)) != null)) )
        )
        | map(select(.context? != null)) | unique')
  # Heal a stale matrix-PARENT context. When a CI job is refactored from a single
  # job (check run "Build") into a matrix (runs "Build (cmd/api)", "Build (…)"),
  # the bare "Build" is NEVER reported as a check run again — but the additive
  # union above keeps requiring it, so every PR blocks forever on a check that
  # can't complete (enforce_admins means not even an admin merge escapes). Prune a
  # required context C ONLY with POSITIVE evidence it has vanished: discovery
  # shows a matrix child "C (…)", AND bare C is confirmed ABSENT from the default
  # branch HEAD's own check-runs ($head_names). Name-prefix alone is too weak —
  # discovery is bounded to recent PR runs, so an independent required "C" can be
  # absent from discovery while an UNRELATED "C (…)" is present; keying off that
  # would strip a still-valid gate. Requiring C to also be missing from the fresh
  # HEAD check-run set rules that out: a real, active C still runs on main and
  # stays in $head_names, so only a genuinely refactored-away parent is pruned. No
  # HEAD evidence (empty $head_names) → preserve (fail-open, never strip).
  want=$(printf '%s\n%s\n%s' "$want" "$desired" "$head_names" | jq -sc '
    .[0] as $want | (.[1] | map(.context)) as $dctx | .[2] as $head |
    $want | map(
      (.context) as $c |
      select(
        ($dctx | index($c)) != null                       # itself a discovered check → keep
        or (($dctx | any(startswith($c + " ("))) | not)   # no matrix child shadows it → keep
        or (($head | length) == 0)                        # no HEAD evidence → keep (fail-open)
        or (($head | index($c)) != null)                  # still runs on main HEAD → keep (not stale)
      )
    )')
elif [ -n "$current" ] && [ "$current" != "[]" ]; then
  want="$current"
else
  want="[]"
fi

# Already exactly how we want it (PR-mode + admins + matching checks)? Skip.
if [ "$has_reviews" = "true" ] && [ "$has_admins" = "true" ] && [ "${current:-[]}" = "$want" ]; then
  exit 0
fi

if [ "$want" = "[]" ]; then
  rsc='null'
  echo "github-guard: protecting $slug:$branch (require PR, enforce admins, linear history)…" >&2
else
  rsc="{ \"strict\": true, \"checks\": $want }"
  echo "github-guard: protecting $slug:$branch (require PR, enforce admins, linear history, required checks $want)…" >&2
fi

payload=$(cat <<JSON
{
  "required_status_checks": $rsc,
  "enforce_admins": true,
  "required_pull_request_reviews": { "required_approving_review_count": $review_count, "dismiss_stale_reviews": false, "require_code_owner_reviews": false },
  "restrictions": null,
  "required_linear_history": true,
  "allow_force_pushes": false,
  "allow_deletions": false
}
JSON
)
if printf '%s' "$payload" | gh api -X PUT "repos/$slug/branches/$branch/protection" \
     -H "Accept: application/vnd.github+json" --input - >/dev/null 2>&1; then
  echo "github-guard: $slug:$branch protected ✓" >&2
else
  echo "github-guard: protection PUT failed for $slug:$branch (need repo admin?) — not blocking" >&2
fi
exit 0
