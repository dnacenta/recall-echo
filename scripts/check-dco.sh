#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Fail when a non-merge commit in <base>..<head> has no Signed-off-by trailer
# equal to that commit's author. Extra trailers are ignored. Email comparison
# is case-sensitive because the match is the whole author identity.
#
#   scripts/check-dco.sh origin/main HEAD
#
# CI passes the base branch and head sha through the environment (no
# pull-request-controlled text in the workflow script):
#
#   DCO_BASE_REF=main DCO_HEAD_SHA=<sha> scripts/check-dco.sh

set -euo pipefail

usage() {
  printf '%s\n' \
    "usage: scripts/check-dco.sh <base> <head>" \
    "   or: DCO_BASE_REF=<branch> DCO_HEAD_SHA=<sha> scripts/check-dco.sh" >&2
}

if [[ $# -eq 2 ]]; then
  base=$1
  head=$2
elif [[ $# -eq 0 && -n ${DCO_BASE_REF:-} && -n ${DCO_HEAD_SHA:-} ]]; then
  base="origin/${DCO_BASE_REF}"
  head=$DCO_HEAD_SHA
else
  usage
  exit 2
fi

if ! git rev-parse --git-dir >/dev/null 2>&1; then
  printf 'dco: not a git repository\n' >&2
  exit 2
fi

if ! git rev-parse --verify --quiet "${base}^{commit}" >/dev/null; then
  printf 'dco: base revision not found: %s\n' "$base" >&2
  exit 2
fi

if ! git rev-parse --verify --quiet "${head}^{commit}" >/dev/null; then
  printf 'dco: head revision not found: %s\n' "$head" >&2
  exit 2
fi

# True when one Signed-off-by value is exactly the author identity.
trailer_matches_author() {
  local trailers=$1
  local author=$2
  local line
  while IFS= read -r line || [[ -n $line ]]; do
    if [[ -n $line && $line == "$author" ]]; then
      return 0
    fi
  done <<<"$trailers"
  return 1
}

failures=()
checked=0

# rev-list failing (bad range) must not look like a clean sign-off.
commits=$(git rev-list --reverse --no-merges "${base}..${head}") || exit 2

if [[ -n $commits ]]; then
  while IFS= read -r commit; do
    [[ -z $commit ]] && continue
    checked=$((checked + 1))
    author=$(git log -1 --format='%an <%ae>' "$commit")
    trailers=$(git log -1 --format='%(trailers:key=Signed-off-by,valueonly)' "$commit")
    if ! trailer_matches_author "$trailers" "$author"; then
      short=$(git log -1 --format='%h' "$commit")
      subject=$(git log -1 --format='%s' "$commit")
      failures+=("${short} ${subject}")
    fi
  done <<<"$commits"
fi

if [[ ${#failures[@]} -eq 0 ]]; then
  printf 'dco: ok (%s non-merge commits in %s..%s)\n' "$checked" "$base" "$head"
  exit 0
fi

printf 'dco: missing or mismatched Signed-off-by (must equal the commit author):\n'
for line in "${failures[@]}"; do
  printf '  %s\n' "$line"
done
printf '\n'
printf 'Fix a missing sign-off:\n'
printf '  last commit: git commit --amend -s --no-edit\n'
printf '  several commits: git rebase --signoff main\n'
printf '    or: git rebase -i with exec git commit --amend -s --no-edit\n'
printf '  then: git push --force-with-lease to the pull request branch\n'
exit 1
