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
  # The Cargo workspace is the repository root: crates/core and the src-tauri shell.
  cd "$repo_root"

  cargo test --workspace
  cargo check --workspace --all-targets
  cargo check --workspace --no-default-features

  local feature pkg
  for feature in ai edit raw instagram collage slideshow localsend map faces smarttags flickr smugmug; do
    for pkg in chairphoto-core chairphoto; do
      RUSTFLAGS="-D warnings" \
        cargo check -p "$pkg" --no-default-features --features "$feature" --all-targets
    done
  done

  RUSTFLAGS="-D warnings" cargo check --workspace --all-targets
  cargo check --workspace --all-features --all-targets
}

run_frontend_checks
run_backend_checks
