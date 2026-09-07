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
      # A *_REF indirection: resolve every REF variable too.
      refs=$(grep -rhoE "^[[:space:]]*[A-Z0-9_]+_REF:[[:space:]]*v[0-9]+\.[0-9]+\.[0-9]+" \
               .github/workflows/*.yml chores.yml 2>/dev/null | grep -oE "v[0-9]+\.[0-9]+\.[0-9]+" | sort -u)
      # Nothing pinned for this sibling at all → not this check's business.
      [ -n "$pins$refs" ] || continue

      # EVERY PIN THAT CLONES THIS SIBLING INTO THE CONSUMER'S OWN
      # SIBLING SLOT MUST MATCH, not merely one of them.
      #
      # This check used to be existential -- it passed as soon as any
      # workflow named the lock's version. That let a repository hold two
      # pins for the same sibling and be told it was fine: `ci.yml` at
      # v0.2.10 satisfied the check while `release.yml` sat at v0.2.7,
      # and since the release job only runs on a tag, the first sign was
      # a failed publish. The guard was in place the whole time and could
      # not have caught it, which is worse than not having it.
      #
      # `../<sibling>` IS THE DISCRIMINATOR, and it matters. A workflow
      # may legitimately clone the same sibling somewhere else, at a
      # different version, to satisfy a DIFFERENT crate's requirement --
      # `rust-fs-ntfs` clones am-fs-core into RUNNER_TEMP at the version
      # am-img-vhd wants, which has nothing to do with this crate's lock.
      # Flagging that would be a false positive, and a guard that cries
      # wolf gets bypassed.
      #
      # TWO SPELLINGS, and both have to be read. A `git clone --branch`
      # puts the tag and the destination on one line, so a line filter
      # sees it. `actions/checkout` spreads `repository:`, `ref:` and
      # `path:` over separate lines, and a line filter sees none of
      # them -- which is how a second repository in this constellation
      # ran this check green with two pins it never examined.
      # THREE SPELLINGS, and the third is the one that hid the bug this
      # check was written for. A clone line may carry the tag as a
      # literal, or as a variable -- `--branch "$FS_CORE_REF"` -- and
      # reading only literals means the guard goes quiet exactly when a
      # repository does the tidier thing.
      #
      # That is not a hypothetical ordering either. The fix for the
      # original drift REPLACED release.yml's literal with the variable,
      # so a check that reads literals only was blinded by the very
      # commit that repaired the thing it was meant to catch. Verified:
      # with `FS_CORE_REF: v0.2.7` in release.yml and the lock at
      # 0.2.10, the literal-only version exits 0 and prints nothing.
      bad=""
      chores_file=""
      [ -f chores.yml ] && chores_file=chores.yml
      for wf in .github/workflows/*.yml .github/workflows/*.yaml; do
        [ -f "$wf" ] || continue
        wf_bad=$(awk -v sib="$sib" -v want="v$lockver" -v file="$wf" '
          # `chores.yml` first, then this file`s own env:, so a name
          # defined in both resolves to the workflow`s value.
          #
          # WHY chores.yml IS READ HERE. A repository may assign the pin
          # from a shell helper rather than a workflow variable --
          # `core_ref="$(pin FS_CORE_REF)"`, with the value in
          # chores.yml -- which is a BETTER design than a per-file env:
          # block, because one declaration then feeds every clone. It is
          # also the design this resolver could not see, so the tidiest
          # repository in the group was the one going unchecked. That is
          # the third spelling this check has had to learn, and each
          # time the blind spot was a repository doing something more
          # careful than the ones it already handled.
          FNR == NR {
            if ($0 ~ /^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*:[[:space:]]*v[0-9]+\.[0-9]+\.[0-9]+[[:space:]]*$/) {
              key = $1; sub(/:$/, "", key); env[key] = $2
            }
            next
          }
          # This file`s own env: assignments, which win over chores.yml.
          /^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*:[[:space:]]*v[0-9]+\.[0-9]+\.[0-9]+[[:space:]]*$/ {
            key = $1; sub(/:$/, "", key); val = $2; env[key] = val
          }
          # ONE HOP THROUGH A SHELL ASSIGNMENT:
          #
          #   core_ref="$(pin FS_CORE_REF)"
          #
          # The clone then reads `$core_ref`, whose value comes from
          # `FS_CORE_REF` in chores.yml. Without this the guard can see
          # that it cannot resolve the name but not what the name means,
          # and reports a shrug where it could report an answer.
          /^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*=.*\$\(pin[[:space:]]+[A-Za-z_][A-Za-z0-9_]*[[:space:]]*\)/ {
            lhs = $0
            sub(/^[[:space:]]*/, "", lhs)
            sub(/=.*$/, "", lhs)
            rhs = $0
            sub(/^.*\$\(pin[[:space:]]+/, "", rhs)
            sub(/[[:space:]]*\).*$/, "", rhs)
            if (rhs in env) { env[lhs] = env[rhs] }
          }
          # JOIN BACKSLASH CONTINUATIONS FIRST. A clone is routinely
          # written across several physical lines:
          #
          #   git clone --quiet --depth 1 --branch "$core_ref" \\
          #       https://…/rust-fs-core.git ../rust-fs-core
          #
          # so `--branch` and `../<sibling>` are never on the same line
          # and a per-line match sees neither. That is the fourth
          # spelling this check has had to learn, and it was silent
          # rather than wrong -- the pattern matched nothing, so the
          # guard reported nothing.
          {
            if (buf == "") { bufline = FNR }
            line = $0
            if (line ~ /\\[[:space:]]*$/) {
              sub(/\\[[:space:]]*$/, "", line)
              buf = buf line " "
              next
            }
            buf = buf line
            logical = buf
            buf = ""
          }
          # A clone whose destination is the consumer`s own sibling slot.
          logical ~ ("\\.\\./" sib "([^A-Za-z0-9._-]|$)") && logical ~ /--branch/ {
            ref = ""
            n = split(logical, tok, /[[:space:]]+/)
            for (i = 1; i <= n; i++) {
              if (tok[i] == "--branch" || tok[i] == "-b") { ref = tok[i + 1]; break }
            }
            gsub(/["\047]/, "", ref)
            if (ref ~ /^\$\{?[A-Za-z_][A-Za-z0-9_]*\}?$/) {
              name = ref
              gsub(/[$\{\}]/, "", name)
              resolved = (name in env) ? env[name] : ""
              # AN UNRESOLVABLE NAME IS REPORTED, not skipped. The
              # previous version dropped it silently, which meant the
              # guard`s quietest output -- nothing at all -- covered
              # both "this pin is correct" and "this pin was never
              # read". Those are not the same answer and must not look
              # the same. Naming it is noisy in the rare case the
              # variable comes from somewhere this script cannot see;
              # that is the right way round, because the failure of a
              # silent skip is invisible and the failure of a false
              # alarm is a person reading one line.
              if (resolved == "") {
                printf "%s:%d: --branch %s (cannot resolve; not checked)\n", file, bufline, ref
                next
              }
              if (resolved != want) {
                printf "%s:%d: --branch %s = %s\n", file, bufline, ref, resolved
              }
              next
            }
            if (ref ~ /^v[0-9]+\.[0-9]+\.[0-9]+$/ && ref != want) {
              printf "%s:%d: --branch %s\n", file, bufline, ref
            }
          }
        ' "${chores_file:-/dev/null}" "$wf")
        [ -n "$wf_bad" ] && bad="${bad:+$bad
}$wf_bad"
      done

      # The `actions/checkout` form. A `repository:` naming the sibling,
      # then within the next few lines a `ref:` giving the tag and a
      # `path:` giving where it lands. `path` is the discriminator here,
      # exactly as `../<sib>` is above: a checkout of the same sibling
      # somewhere else, to satisfy some other crate, is not this lock's
      # business.
      for wf in .github/workflows/*.yml .github/workflows/*.yaml; do
        [ -f "$wf" ] || continue
        checkout_bad=$(awk -v sib="$sib" -v want="v$lockver" -v file="$wf" '
          $0 ~ ("repository:[[:space:]]*.*/" sib "[[:space:]]*$") { inblk = 1; ln = NR; ref = ""; pth = ""; next }
          inblk {
            if ($0 ~ /ref:[[:space:]]*v[0-9]+\.[0-9]+\.[0-9]+/) { ref = $NF }
            if ($0 ~ /path:[[:space:]]*/) { pth = $NF }
            if (NR - ln > 4 || ($0 ~ /^[[:space:]]*-/ && NR > ln)) {
              if (ref != "" && ref != want && (pth == sib || pth == ".." "/" sib)) {
                printf "%s:%d: ref: %s\n", file, ln, ref
              }
              inblk = 0
            }
          }
          END {
            if (inblk && ref != "" && ref != want && (pth == sib || pth == ".." "/" sib)) {
              printf "%s:%d: ref: %s\n", file, ln, ref
            }
          }
        ' "$wf")
        [ -n "$checkout_bad" ] && bad="${bad:+$bad
}$checkout_bad"
      done
      if [ -z "$bad" ]; then
        # Every literal that clones into the sibling slot agrees. The
        # remaining way to be pinned is through a *_REF variable.
        if [ -n "$(printf '%s\n' "$pins" | grep -E "^v${lockver}$")" ] \
           || [ -n "$(printf '%s\n' "$refs" | grep -E "^v${lockver}$")" ]; then
          continue
        fi
      fi
      echo "[deps] Cargo.lock records $crate = $lockver, and a workflow pins a different version." >&2
      if [ -n "$bad" ]; then
        echo "       These lines clone ../$sib at a version the lock does not name:" >&2
        printf '         %s\n' "$bad" >&2
      else
        echo "       The workflows pin: $(printf '%s\n' $pins $refs | sort -u | tr '\n' ' ')" >&2
      fi
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
