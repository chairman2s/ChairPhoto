#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

run_backend_checks() {
  # The Cargo workspace is the repository root: crates/core, crates/model and crates/app.
  cd "$repo_root"

  cargo test --workspace
  cargo check --workspace --all-targets
  cargo check --workspace --no-default-features

  local feature pkg
  for feature in ai edit raw instagram collage slideshow localsend map faces smarttags flickr smugmug; do
    for pkg in chairphoto-core chairphoto-app; do
      RUSTFLAGS="-D warnings" \
        cargo check -p "$pkg" --no-default-features --features "$feature" --all-targets
    done
  done

  # tag-graph is app-only (crates/app/Cargo.toml has no matching chairphoto-core feature),
  # so it checks against chairphoto-app alone rather than joining the loop above.
  RUSTFLAGS="-D warnings" \
    cargo check -p chairphoto-app --no-default-features --features tag-graph --all-targets

  RUSTFLAGS="-D warnings" cargo check --workspace --all-targets
  cargo check --workspace --all-features --all-targets
}

run_backend_checks
