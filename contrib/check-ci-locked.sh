#!/usr/bin/env bash
# Fail if any cargo build/test/clippy invocation in CI omits --locked.
set -euo pipefail
cd "$(dirname "$0")/.."
bad=$(grep -nE 'cargo (build|test|clippy)' .github/workflows/ci.yml | grep -v -- '--locked' || true)
if [ -n "$bad" ]; then
  echo "ci.yml cargo invocations missing --locked:" >&2
  echo "$bad" >&2
  exit 1
fi
