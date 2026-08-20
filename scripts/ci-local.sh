#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

run_frontend_checks() {
  cd "$repo_root"

  npm ci
  npx tsc --noEmit
  npm test
  npm run build
}

run_backend_checks() {
  cd "$repo_root/src-tauri"

  cargo test
  cargo check --all-targets
  cargo check --no-default-features

  local feature
  for feature in ai edit raw instagram collage slideshow localsend map faces smarttags flickr smugmug; do
    RUSTFLAGS="-D warnings" \
      cargo check --no-default-features --features "$feature" --all-targets
  done

  RUSTFLAGS="-D warnings" cargo check --all-targets
  cargo check --all-features --all-targets
}

run_frontend_checks
run_backend_checks
