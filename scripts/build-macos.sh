#!/bin/sh
set -eu
[ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] || { echo 'This candidate targets Apple Silicon macOS.' >&2; exit 1; }
workflow_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$workflow_root"
workflow_cli=${WORKFLOW_TAURI_CLI:-target/devtools/node_modules/.bin/tauri}
[ -x "$workflow_cli" ] || { echo 'Install the Tauri 2 CLI as a development tool first; no installation was performed.' >&2; exit 1; }
workflow_separator=$(printf '\037')
CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$HOME=/build-home${workflow_separator}--remap-path-prefix=$workflow_root=/workflow${workflow_separator}--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo${workflow_separator}--remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup"
export CARGO_ENCODED_RUSTFLAGS
export MACOSX_DEPLOYMENT_TARGET=14.0
# This command produces a local ad-hoc candidate. Publication and notarization are separate actions.
env -u APPLE_ID -u APPLE_PASSWORD -u APPLE_TEAM_ID -u APPLE_API_KEY -u APPLE_API_ISSUER -u APPLE_API_KEY_PATH \
  "$workflow_cli" build --bundles app --ci --features custom-protocol -- --locked "$@"
