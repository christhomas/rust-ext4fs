#!/usr/bin/env bash
# guard: git-block-merge-commits
# Block any push whose pushed range contains a merge commit — the safety net
# behind git-block-merge-commit (also catches merges that arrived via fetch,
# cherry-pick, or a merge-preserving rebase). Reads the ref list git provides
# on stdin. Purely local. Hard block. Bypass only with --no-verify.
set -u
status=0
while read -r local_ref local_sha remote_ref remote_sha; do
  # Deleting a remote branch (local_sha all-zero): nothing to inspect.
  printf '%s' "$local_sha" | grep -qE '^0+$' && continue
  # Inspect only the merge commits this push actually INTRODUCES: those
  # reachable from the new head but not from any remote-tracking ref.
  #
  # A "<remote_sha>..<local_sha>" range looks equivalent and is not. After
  # a rebase the old remote tip is no longer an ancestor of the new head,
  # so that range means "everything reachable from the new head but not
  # the old tip" — which sweeps in the entire new base, including merge
  # commits already published on it. The guard then blocks a force-push
  # that introduces no merges at all, which is the normal way to update a
  # rebased branch.
  #
  # Excluding --remotes is correct for a fast-forward too: anything the
  # old tip could reach is on a remote ref by definition.
  merges=$(git rev-list --merges "$local_sha" --not --remotes 2>/dev/null)
  if [ -n "$merges" ]; then
    printf 'github-guard: BLOCKED — push to %s contains merge commit(s):\n' "$remote_ref" >&2
    printf '%s\n' "$merges" | sed 's/^/  /' >&2
    printf '  Linearize first:  git pull --rebase   |   git rebase <upstream>\n' >&2
    status=1
  fi
done
exit $status
