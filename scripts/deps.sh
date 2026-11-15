#!/usr/bin/env bash
# Fetch the two public dependencies at pinned commits into .deps/ (once).
set -euo pipefail
cd "$(dirname "$0")/.."
fetch() { # name commit
  local dir=".deps/$1"
  if [ ! -d "$dir/.git" ]; then
    git clone -q "https://github.com/ce-net/$1" "$dir"
  fi
  if [ "$(git -C "$dir" rev-parse HEAD)" != "$2" ]; then
    git -C "$dir" fetch -q origin
    git -C "$dir" checkout -q "$2"
  fi
}
fetch ce-net-task-worker-loop 414b264b014abc6caa81b1dad7022316f41b1ba6
fetch ce-net-local-dev-sim 0dd3be88460ae717b1ad3ac31bd698a0574c87f8
