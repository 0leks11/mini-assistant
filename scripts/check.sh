#!/usr/bin/env bash
# Host-only quality checks. Never source .env or launch the assistant here.
set -euo pipefail

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd -- "$SCRIPT_DIR/.."

cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python3 -B -m unittest discover -s tests -p 'test_*.py' -v
