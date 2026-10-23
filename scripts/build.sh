#!/usr/bin/env bash
#
# Build streamripper-rs with a bit of ceremony: toolchain check, format check,
# clippy, tests, then the actual build, and a report of the resulting binary.
#
# Usage:
#   bash scripts/build.sh                 # release build, with checks + tests
#   bash scripts/build.sh --debug         # debug build instead of release
#   bash scripts/build.sh --fast          # skip fmt check, clippy and tests
#   bash scripts/build.sh --no-clippy     # skip clippy
#   bash scripts/build.sh --no-test       # skip the test run
#   bash scripts/build.sh --cross <triple>  # cross-compile (e.g. x86_64-unknown-linux-musl)
#   bash scripts/build.sh --clean         # cargo clean first
#   bash scripts/build.sh --run -- <args> # run the built binary with <args> afterwards
#   bash scripts/build.sh --verbose       # stream tool output instead of spinners
#
# The format check is advisory (a warning); clippy and tests are gates unless
# skipped.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# shellcheck source=scripts/lib/ui.sh
source scripts/lib/ui.sh

PROFILE="release"
DO_FMT=1
DO_CLIPPY=1
DO_TEST=1
DO_CLEAN=0
DO_RUN=0
CROSS=""
RUN_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --debug)     PROFILE="debug"; shift ;;
    --release)   PROFILE="release"; shift ;;
    --fast)      DO_FMT=0; DO_CLIPPY=0; DO_TEST=0; shift ;;
    --no-fmt)    DO_FMT=0; shift ;;
    --no-clippy) DO_CLIPPY=0; shift ;;
    --no-test)   DO_TEST=0; shift ;;
    --clean)     DO_CLEAN=1; shift ;;
    --cross)     CROSS="${2:?--cross needs a target triple}"; shift 2 ;;
    --run)       DO_RUN=1; shift; RUN_ARGS=("$@"); break ;;
    --verbose)   export SR_VERBOSE=1; shift ;;
    -h|--help)   sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) ui_err "unknown option: $1 (see --help)"; exit 2 ;;
  esac
done

# --- locate cargo (handle Homebrew's keg-only rustup, which isn't on PATH) ---
CARGO=""
for c in cargo "$HOME/.cargo/bin/cargo" /opt/homebrew/opt/rustup/bin/cargo /usr/local/opt/rustup/bin/cargo; do
  if command -v "$c" >/dev/null 2>&1; then CARGO="$(command -v "$c")"; break; fi
done
[[ -n "$CARGO" ]] || ui_die "cargo not found. Install Rust: https://rustup.rs (or: brew install rustup && rustup default stable)"
# Put cargo's own directory (for cargo-fmt/cargo-clippy, needed with Homebrew's
# keg-only rustup) and ~/.cargo/bin (for `cargo install`ed subcommands such as
# cargo-zigbuild) on PATH.
export PATH="$HOME/.cargo/bin:$(dirname "$CARGO"):$PATH"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
BUILD_FLAGS=()
[[ "$PROFILE" == "release" ]] && BUILD_FLAGS+=(--release)
TARGET_DETAIL="$("$CARGO" -V 2>/dev/null | awk '{print $1, $2}')"
if [[ -n "$CROSS" ]]; then
  BUILD_FLAGS+=(--target "$CROSS")
  OUT_DIR="target/$CROSS/$PROFILE"
  DETAIL="$PROFILE  |  $CROSS"
else
  OUT_DIR="target/$PROFILE"
  DETAIL="$PROFILE  |  host ($(uname -m)-$(uname -s | tr '[:upper:]' '[:lower:]'))"
fi

ui_banner "rust port of streamripper  ·  v$VERSION" "$DETAIL  ·  $TARGET_DETAIL"

# --- toolchain ---
ui_section "Toolchain"
ui_info "$("$CARGO" -V)"
ui_info "$("${CARGO%cargo}rustc" -V 2>/dev/null || "$CARGO" rustc -- -V 2>/dev/null || true)"

if [[ -n "$CROSS" ]]; then
  if ! ("${CARGO%cargo}rustup" target list --installed 2>/dev/null | grep -qx "$CROSS"); then
    ui_warn "target $CROSS is not installed — add it with: rustup target add $CROSS"
    ui_warn "(a cross build also needs a matching linker; e.g. musl-cross from Homebrew)"
  fi
fi

[[ "$DO_CLEAN" == 1 ]] && { ui_section "Clean"; ui_run clean "cargo clean" "$CARGO" clean; }

# --- checks ---
if [[ "$DO_FMT" == 1 || "$DO_CLIPPY" == 1 || "$DO_TEST" == 1 ]]; then
  ui_section "Checks"
  if [[ "$DO_FMT" == 1 ]]; then
    if ui_run fmt "Format check (cargo fmt --check)" "$CARGO" fmt --all -- --check; then :; else
      ui_warn "code is not rustfmt-clean — run 'cargo fmt' to fix (continuing)"
    fi
  fi
  if [[ "$DO_CLIPPY" == 1 ]]; then
    ui_run clippy "Clippy (-D warnings)" "$CARGO" clippy --all-targets -- -D warnings \
      || ui_die "clippy found issues (see above)"
  fi
  if [[ "$DO_TEST" == 1 ]]; then
    ui_run test "Tests (cargo test)" "$CARGO" test \
      || ui_die "tests failed (see above)"
  fi
fi

# --- build ---
ui_section "Build"
# For a Linux cross target, prefer cargo-zigbuild (uses zig as the C
# compiler/linker, so ring's C builds and links cleanly from macOS). Fall back
# to plain `cargo build --target` when zigbuild isn't installed.
CARGO_SUB="build"
if [[ -n "$CROSS" && "$CROSS" == *linux* ]] && command -v cargo-zigbuild >/dev/null 2>&1; then
  CARGO_SUB="zigbuild"
  ui_info "using cargo-zigbuild for $CROSS"
fi
ui_run build "Compiling ($PROFILE)" "$CARGO" "$CARGO_SUB" ${BUILD_FLAGS[@]+"${BUILD_FLAGS[@]}"} \
  || ui_die "build failed (see above)"

# --- report ---
BIN="$OUT_DIR/streamripper-rs"
if [[ -f "$BIN" ]]; then
  SIZE="$(du -h "$BIN" | awk '{print $1}')"
  ui_ok "binary: $BIN  ($SIZE)"
else
  ui_warn "expected binary not found at $BIN"
fi

ui_done "streamripper-rs $VERSION" "$PROFILE build"

# --- optional run ---
if [[ "$DO_RUN" == 1 ]]; then
  ui_section "Run"
  ui_step "$BIN ${RUN_ARGS[*]:-}"
  exec "$BIN" ${RUN_ARGS[@]+"${RUN_ARGS[@]}"}
fi
