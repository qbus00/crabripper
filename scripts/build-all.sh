#!/usr/bin/env bash
#
# Build streamripper-rs for all three release architectures — the same set the C
# streamripper fork ships — into dist/:
#   dist/streamripper-rs-macos-arm64   (aarch64-apple-darwin, native)
#   dist/streamripper-rs-linux-amd64   (x86_64-unknown-linux-musl, fully static)
#   dist/streamripper-rs-linux-arm64   (aarch64-unknown-linux-musl, fully static)
#
# The Linux targets cross-compile from macOS via cargo-zigbuild (zig supplies the
# C toolchain that ring needs), producing fully-static binaries with no Docker,
# no QEMU and no OpenSSL.
#
# Usage:
#   bash scripts/build-all.sh            # all three, release, + .gz copies
#   bash scripts/build-all.sh --no-gzip  # skip the .gz copies
#   bash scripts/build-all.sh --verbose  # stream tool output
#
# Linux targets need: rustup target add <triple>, cargo install cargo-zigbuild,
# brew install zig. Missing pieces are reported (with the fix) and skipped.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
# shellcheck source=scripts/lib/ui.sh
source scripts/lib/ui.sh

GZIP=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-gzip) GZIP=0; shift ;;
    --verbose) export SR_VERBOSE=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) ui_err "unknown option: $1 (see --help)"; exit 2 ;;
  esac
done

CARGO=""
for c in cargo "$HOME/.cargo/bin/cargo" /opt/homebrew/opt/rustup/bin/cargo /usr/local/opt/rustup/bin/cargo; do
  if command -v "$c" >/dev/null 2>&1; then CARGO="$(command -v "$c")"; break; fi
done
[[ -n "$CARGO" ]] || ui_die "cargo not found (install Rust via rustup)"
export PATH="$HOME/.cargo/bin:$(dirname "$CARGO"):$PATH"
RUSTUP="${CARGO%cargo}rustup"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
mkdir -p dist

ui_banner "all targets  ·  v$VERSION" "macos-arm64  ·  linux-amd64  ·  linux-arm64"

# triple | dist-label | method(native|zig)
TARGETS="
aarch64-apple-darwin|macos-arm64|native
x86_64-unknown-linux-musl|linux-amd64|zig
aarch64-unknown-linux-musl|linux-arm64|zig
"

built=0
while IFS='|' read -r triple label method; do
  [[ -z "$triple" ]] && continue
  ui_section "$label  ($triple)"

  if command -v "$RUSTUP" >/dev/null 2>&1 \
     && ! "$RUSTUP" target list --installed 2>/dev/null | grep -qx "$triple"; then
    ui_warn "rust target not installed — run: rustup target add $triple"
    continue
  fi

  sub="build"
  if [[ "$method" == "zig" ]]; then
    if command -v cargo-zigbuild >/dev/null 2>&1; then
      sub="zigbuild"
    else
      ui_warn "cargo-zigbuild missing (needed for $label) — run: brew install zig && cargo install cargo-zigbuild"
      continue
    fi
  fi

  if ui_run "build-$label" "Compiling $label (release)" "$CARGO" "$sub" --release --target "$triple"; then
    out="dist/streamripper-rs-$label"
    cp "target/$triple/release/streamripper-rs" "$out"
    ui_ok "$out  ($(du -h "$out" | awk '{print $1}'))"
    if [[ "$GZIP" == 1 ]]; then gzip -kf "$out"; ui_info "gzipped: $out.gz"; fi
    built=$((built + 1))
  fi
done <<EOF
$TARGETS
EOF

ui_done "streamripper-rs $VERSION" "$built/3 targets -> dist/"
