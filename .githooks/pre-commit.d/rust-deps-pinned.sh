#!/usr/bin/env bash
# guard: rust-deps-pinned
# Reproducible-release gate for Cargo projects: refuse a commit that would make
# a versioned tag non-reproducible. Only runs when Cargo.toml is present (skips
# silently otherwise), so non-Rust repos are unaffected. BLOCKS on a real
# problem; fail-OPEN whenever the environment can't give a reliable answer (no
# cargo, empty offline cache, a path-dep sibling not checked out) — CI's
# `cargo … --locked` is the authoritative backstop.
#
# It catches the ways a "pinned" release silently isn't:
#   1. A workflow that FLOATING-clones a sibling repo (same GitHub owner) with
#      no `--branch`/`-b` — the published build then resolves against whatever
#      that repo's HEAD happens to be, not a fixed tag.
#   2. A workflow `actions/checkout` of a sibling repo with no `ref:` — same
#      floating hazard, via the action instead of raw git.
#   3. A committed Cargo.lock whose own package version drifted from Cargo.toml
#      (bumped the manifest, forgot to re-lock — only blows up at `cargo publish`).
#   4. A Cargo.lock that `cargo metadata --locked` reports as stale.
#
# Bypass once (NOT recommended): git commit --no-verify
set -u
dir=$(cd "$(dirname "$0")/.." && pwd)   # .githooks/
# shellcheck source=../lib/common.sh
. "$dir/lib/common.sh"

gg_is_rust || exit 0
root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0
cd "$root" || exit 0
fail=0

# ── 1 & 2. no FLOATING fetch of a sibling repo (same owner) in any workflow ──
# A "sibling" is another repo under the same GitHub owner as origin — those are
# the path/git dependency crates a release must pin. If the owner can't be
# determined (no github origin) we skip these two checks; the lock checks below
# still run.
owner=$(gg_repo_slug); owner=${owner%%/*}
if [ -n "$owner" ] && [ -d .github/workflows ]; then
  shopt -s nullglob 2>/dev/null || true
  for wf in .github/workflows/*.yml .github/workflows/*.yaml; do
    [ -f "$wf" ] || continue
    # Join shell line-continuations (a trailing '\') into one logical line before
    # scanning: in a `run: |` block a `git clone …\` and its `--branch vX` land on
    # two physical lines, and a per-physical-line check would flag the first as
    # unpinned even though the clone is pinned.
    logical=""
    while IFS= read -r line || [ -n "$line" ]; do
      line=${line%$'\r'}
      logical="${logical:+$logical }$line"
      case "$logical" in *\\) logical=${logical%\\}; continue ;; esac   # keep joining
      case "$logical" in
        *"git clone"*github.com[:/]"$owner"/*)
          # A real pin is an IMMUTABLE ref. Pull out the --branch/-b value: empty
          # means no pin at all, and a well-known MUTABLE branch name (main/master/
          # …) is just as floating as none — block both; anything else (a tag) ok.
          brval=$(printf '%s' "$logical" | sed -nE 's/.*(^|[[:space:]])(--branch[ =]|-b[[:space:]]*)([^[:space:]]+).*/\3/p')
          brval=${brval//\"/}; brval=${brval//\'/}     # strip surrounding quotes
          # Heuristic blocklist of well-known mutable branch names (not exhaustive
          # by design — the authoritative reproducibility gate is the Cargo.lock
          # checks below + CI's --locked; this just catches the common offenders).
          # Matched case-insensitively so Main/MAIN/Develop are caught too.
          case "$(printf '%s' "$brval" | tr '[:upper:]' '[:lower:]')" in
            main | master | develop | dev | trunk | head | next | staging | release | canary)
              echo "[deps] FLOATING git clone — --branch '$brval' is a MUTABLE branch (pin a tag) in $wf:" >&2
              echo "       ${logical#"${logical%%[![:space:]]*}"}" >&2
              fail=1 ;;
            '')
              echo "[deps] FLOATING git clone of a sibling repo (add --branch v<X>) in $wf:" >&2
              echo "       ${logical#"${logical%%[![:space:]]*}"}" >&2
              fail=1 ;;
            *) : ;;                                        # pinned to a tag → ok
          esac ;;
      esac
      logical=""
    done < "$wf"
  done

  # actions/checkout of an explicit sibling repository: with no ref:. Parsed
  # with ruby's stdlib YAML when available; skipped (never failed) if ruby isn't.
  if command -v ruby >/dev/null 2>&1; then
    ruby -ryaml -e '
      # aliases: is a ruby >= 2.7 kwarg; on older rubies (e.g. macOS system ruby)
      # passing it raises ArgumentError, which — if rescued straight to nil — would
      # silently skip EVERY workflow and disable this check. Fall back to a plain
      # safe_load there so the ref-pinning check still runs.
      def load_wf(f)
        YAML.safe_load(File.read(f), aliases: true)
      rescue ArgumentError
        (YAML.safe_load(File.read(f)) rescue nil)
      rescue
        nil
      end
      owner = ARGV[0]
      bad = []
      Dir.glob(".github/workflows/*.{yml,yaml}").each do |wf|
        doc = load_wf(wf)
        next unless doc.is_a?(Hash)
        (doc["jobs"] || {}).each_value do |job|
          next unless job.is_a?(Hash)
          (job["steps"] || []).each do |st|
            next unless st.is_a?(Hash)
            next unless st["uses"].to_s.start_with?("actions/checkout")
            w = st["with"] || {}
            repo = w["repository"].to_s
            next if repo.empty?
            next unless repo.split("/").first == owner        # sibling only
            bad << "#{wf}: actions/checkout #{repo} has no ref: (pin to a tag)" if w["ref"].to_s.empty?
          end
        end
      end
      unless bad.empty?
        STDERR.puts "[deps] FLOATING actions/checkout of a sibling repo (add ref: v<X>):"
        bad.each { |b| STDERR.puts "       #{b}" }
        exit 1
      end
    ' "$owner" || fail=1
  fi
fi

# ── 3. Cargo.lock consistency — only when the repo actually commits a lock ───
# Respect the repo's own convention: enforce the lock if it's tracked; never
# invent one for a library that deliberately gitignores it.
lock_tracked=0; git ls-files --error-unmatch Cargo.lock >/dev/null 2>&1 && lock_tracked=1
if [ "$lock_tracked" = 1 ] && [ ! -f Cargo.lock ]; then
  echo "[deps] Cargo.lock is tracked but missing from the working tree." >&2
  echo "       Restore it: git checkout -- Cargo.lock   (or: cargo generate-lockfile)" >&2
  fail=1
elif [ -f Cargo.lock ]; then
  # Read name/version ONLY from the [package] section. A workspace root, or a
  # manifest carrying [[bin]]/[lib]/[workspace.dependencies] sections, has other
  # `name =`/`version =` keys; a first-match scan would grab the wrong one (or, in
  # a virtual workspace root with no [package], nothing meaningful) and either
  # mis-compare or silently no-op. Section-scoping keeps the drift check honest;
  # a manifest with no [package] (virtual workspace root) correctly yields empty
  # and the guarded block below skips.
  pkg=$(awk -F'"' '
    /^[[:space:]]*\[/ { inpkg = ($0 ~ /^[[:space:]]*\[package\]/) }
    inpkg && /^[[:space:]]*name[[:space:]]*=/ { print $2; exit }
  ' Cargo.toml)
  ver=$(awk -F'"' '
    /^[[:space:]]*\[/ { inpkg = ($0 ~ /^[[:space:]]*\[package\]/) }
    inpkg && /^[[:space:]]*version[[:space:]]*=/ { print $2; exit }
  ' Cargo.toml)
  if [ -n "$pkg" ] && [ -n "$ver" ]; then
    lockver=$(awk -v p="$pkg" '
      $0 == "name = \"" p "\"" { hit=1; next }
      hit && /^version = / { gsub(/^version = "|"$/, "", $0); print; exit }
    ' Cargo.lock)
    if [ -n "$lockver" ] && [ "$lockver" != "$ver" ]; then
      echo "[deps] Cargo.lock records $pkg = $lockver but Cargo.toml is $ver — the lock" >&2
      echo "       drifted from the manifest. Run: cargo generate-lockfile && git add Cargo.lock" >&2
      fail=1
    fi
  fi

  # ── 4. authoritative stale-lock check (cargo is the oracle), best-effort ──
  # `cargo metadata --locked` refuses to rewrite the lock and errors if it's out
  # of date. Run --offline so the hook stays fast and never touches the network.
  #
  # BUT only when the graph resolves the same as CI's. A crate with an EXTERNAL
  # `path = "../sibling"` dependency can't guarantee that locally: a sibling
  # checked out at a version that differs from what the lock records (routine in
  # multi-repo dev) makes cargo want to re-lock, which surfaces as the exact same
  # "cannot update the lock file" error as true staleness — a false block we must
  # not raise. So skip part 4 whenever an external path dep is present; CI (with
  # siblings pinned to their tagged versions) is the authoritative --locked
  # backstop, and parts 1–3 above still apply. Registry-only / in-repo-workspace
  # crates keep the full check.
  ext_path_dep=0
  while IFS= read -r toml; do
    # Anchor `path` to a key boundary (start / space / , / {) so it matches the
    # dependency `path =` key, not a suffix like `manifest-path =`; drop full-line
    # comments so a commented example doesn't count.
    if grep -vE '^[[:space:]]*#' "$toml" 2>/dev/null \
         | grep -qE '(^|[[:space:],{])path[[:space:]]*=[[:space:]]*"\.\.?/'; then ext_path_dep=1; break; fi
  done < <(git ls-files '*Cargo.toml' 'Cargo.toml')
  if [ "$ext_path_dep" = 0 ]; then
    # BLOCK only on the staleness signal; any other failure (empty offline cache,
    # etc.) is an environment limitation → skip. gg_cargo returns 2 with no cargo.
    if err=$(gg_cargo metadata --locked --offline --format-version 1 2>&1 >/dev/null); then
      : # lock is fresh
    else
      rc=$?
      if [ "$rc" != 2 ] && printf '%s\n' "$err" | grep -qiE 'cannot update the lock file|needs to be updated|out.?of.?date'; then
        echo "[deps] Cargo.lock is STALE — it no longer matches Cargo.toml:" >&2
        printf '%s\n' "$err" | grep -iE 'cannot update the lock file|needs to be updated|out.?of.?date' | head -1 | sed 's/^/       /' >&2
        echo "       Fix: cargo generate-lockfile && git add Cargo.lock" >&2
        fail=1
      fi
    fi
  fi
fi

# ── 5. the lock must record the sibling version the workflows PIN ───────────
# The gap part 4 leaves. It skips whenever an external path dep is present,
# deferring to CI's --locked — and that is the one case where the lock and the
# workflow can disagree without anything local noticing.
#
# HOW IT HAPPENS, and it happens constantly in multi-repo work: bump a sibling
# in its own checkout, then run ANY cargo command in a consumer — a build, a
# test, this hook's own clippy — and cargo re-resolves the path dependency and
# rewrites the consumer's lock to the sibling's new version. Commit that, and
# CI clones the sibling at the tag written in the workflow, finds a lock naming
# a version that tag does not have, and stops at:
#
#   error: cannot update the lock file … because --locked was passed
#
# Which is the pin doing its job, several minutes into a run, after a push.
# This says the same thing before the commit exists.
if [ -f Cargo.lock ] && [ -d .github/workflows ]; then
  while IFS= read -r toml; do
    [ -f "$toml" ] || continue
    # `name = { path = "../sibling", … }` — the crate key and the directory it
    # points at, which is the sibling repository's name.
    while IFS='|' read -r crate sib; do
      [ -n "$crate" ] && [ -n "$sib" ] || continue
      lockver=$(awk -v p="$crate" '
        $0 == "name = \"" p "\"" { getline; if ($1 == "version") { gsub(/[":]/, "", $3); print $3; exit } }
      ' Cargo.lock)
      [ -n "$lockver" ] || continue
      # Every tag the workflows name on a line that also names this sibling,
      # plus the same via a *_REF variable defined in the workflow or chores.yml.
      pins=$(grep -rhoE "v[0-9]+\.[0-9]+\.[0-9]+" \
               <(grep -rhE "$sib(\.git)?([^A-Za-z0-9._-]|\$)" .github/workflows/*.yml .github/workflows/*.yaml 2>/dev/null) \
             2>/dev/null | sort -u)
      if printf '%s\n' "$pins" | grep -qE "^v${lockver}$"; then continue; fi
      # A *_REF indirection: resolve every REF variable and try again.
      refs=$(grep -rhoE "^[[:space:]]*[A-Z0-9_]+_REF:[[:space:]]*v[0-9]+\.[0-9]+\.[0-9]+" \
               .github/workflows/*.yml chores.yml 2>/dev/null | grep -oE "v[0-9]+\.[0-9]+\.[0-9]+" | sort -u)
      if printf '%s\n' "$refs" | grep -qE "^v${lockver}$"; then continue; fi
      # Nothing pinned for this sibling at all → not this check's business.
      [ -n "$pins$refs" ] || continue
      echo "[deps] Cargo.lock records $crate = $lockver, and no workflow pins that version." >&2
      echo "       The workflows pin: $(printf '%s\n' $pins $refs | sort -u | tr '\n' ' ')" >&2
      echo "       CI clones the sibling at its pinned tag and then refuses the lock:" >&2
      echo "         error: cannot update the lock file … because --locked was passed" >&2
      echo "       Fix EITHER side: move the pin to v$lockver, or restore the lock" >&2
      echo "       (git checkout HEAD -- Cargo.lock) if the bump was accidental." >&2
      fail=1
    done < <(grep -vE '^[[:space:]]*#' "$toml" 2>/dev/null \
              | sed -nE 's@^[[:space:]]*([A-Za-z0-9_-]+)[[:space:]]*=[[:space:]]*\{[^}]*path[[:space:]]*=[[:space:]]*"\.\.?/([A-Za-z0-9._-]+)".*@\1|\2@p')
  done < <(git ls-files '*Cargo.toml' 'Cargo.toml')
fi

if [ "$fail" != 0 ]; then
  echo "github-guard: rust-deps-pinned blocked the commit — pin your dependencies (above)." >&2
  echo "             Bypass once (NOT recommended): git commit --no-verify" >&2
  exit 1
fi
exit 0
