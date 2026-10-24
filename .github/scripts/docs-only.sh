#!/usr/bin/env bash
# Usage: docs-only.sh BASE_SHA
# Prints docs_only=true to $GITHUB_OUTPUT (or stdout) when every path the
# diff BASE_SHA..HEAD touches is docs/**, changes/**, bench/results/** or a
# *.md file outside crates/; docs_only=false otherwise (WI-2401). Renames
# count as delete + add, so code moved into docs/ is still code. Fails
# safe: no base, a fetch error or an empty diff prints false, so the full
# job runs.
set -uo pipefail
out=${GITHUB_OUTPUT:-/dev/stdout}
base=${1:-}
result=false
if [ -n "$base" ] \
  && { git cat-file -e "$base^{commit}" 2>/dev/null \
       || git fetch --no-tags --depth=1 origin "$base"; } \
  && files=$(git diff --no-renames --name-only "$base" HEAD) \
  && [ -n "$files" ]; then
  result=true
  while IFS= read -r f; do
    case "$f" in
      # A crate may include_str! its own markdown, so crates/** is code.
      crates/*) result=false; break ;;
      docs/* | changes/* | bench/results/* | *.md) ;;
      *) result=false; break ;;
    esac
  done <<<"$files"
fi
echo "docs_only=$result" >>"$out"
echo "docs-only: $result" >&2
