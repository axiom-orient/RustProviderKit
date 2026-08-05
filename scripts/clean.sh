#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

removed_kib=0

remove_path() {
  local path="$1"
  local relative size
  relative=${path#"$ROOT"/}
  if git ls-files --error-unmatch -- "$relative" >/dev/null 2>&1; then
    printf 'FAIL: refusing to remove tracked path: %s\n' "$relative" >&2
    exit 1
  fi
  size=$(du -sk -- "$path" 2>/dev/null | awk '{print $1}')
  removed_kib=$((removed_kib + size))
  rm -rf -- "$path"
  printf 'removed: %s\n' "$relative"
}

if [[ -e "$ROOT/target" ]]; then
  remove_path "$ROOT/target"
fi

while IFS= read -r -d '' path; do
  remove_path "$path"
done < <(
  find "$ROOT" -path "$ROOT/.git" -prune -o -type d \( \
    -name '__pycache__' -o -name '.pytest_cache' \
  \) -print0
)

while IFS= read -r -d '' path; do
  remove_path "$path"
done < <(
  find "$ROOT" -path "$ROOT/.git" -prune -o -type f \( \
    -name '.DS_Store' -o -name '*.log' -o -name '*.tmp' \
  \) -print0
)

printf 'PASS: repository artifacts cleaned (%s KiB removed)\n' "$removed_kib"
