# 🦀 crabripper

A **Rust port of [streamripper](https://github.com/qbus00/streamripper)** — records
shoutcast/icecast radio streams to disk, split into per-track files, with a relay
server, HLS support and more. It aims for **CLI and behaviour parity** with the C
fork while using idiomatic, modern Rust and **as few dependencies as possible**.

> Independent sibling of the C fork; it does not share that project's git history.

## Why

The C streamripper works well, but the Rust port gains:

- **One pure-Rust TLS stack** (rustls) → a **fully-static Linux binary** that
  cross-compiles straight from macOS with **no Docker, no QEMU, no OpenSSL**.
- Memory safety, a clean module layout, and **HTTP redirect following** (which the
  C version lacks — e.g. Radio 357's CDN).

## Status — feature parity

| Feature | Status |
|---|---|
| HTTP + HTTPS streams (TLS via rustls, certificates verified by default) | ✅ |
| ICY inline metadata + track splitting | ✅ |
| Output layout: per-stream subdir, `incomplete/`→complete lifecycle | ✅ |
| Single-file (`-a`), no-individual (`-A`), overwrite modes (`-o`) | ✅ |
| Sequence numbers (`-q`), name patterns (`-D`), keep-incomplete (`-k`) | ✅ |
| Length forms: seconds / `90s`/`12m`/`2h` / `hh:mm[:ss]`, size limit (`-M`) | ✅ |
| ID3 tags (v2.3 prepended, v1 appended; UTF-16 so Polish survives) | ✅ |
| Cue sheets for single-file recordings | ✅ |
| Relay server (`-r`/`-R`/`-z`/`-L`) **with ICY metadata injection** (players show titles) | ✅ |
| HLS (`.m3u8`): master→variant, poll + concatenate segments | ✅ |
| FLAC: native → `.flac`, FLAC-in-Ogg → `.oga`, captured whole | ✅ |
| **Ogg Opus**: per-track split + `OpusTags` titles (capture; decode via symphonia n/a) | ✅ |
| `--wav` (decode → 16-bit PCM WAV): mp3 / aac / ogg-vorbis / flac | ✅ |
| Silence-based splitpoint (`--xs`): mp3, aac | ✅ |
| Ogg per-track splitting (on Ogg logical-stream boundaries) | ✅ |
| HLS AES-128 decryption (`#EXT-X-KEY`) | ✅ |
| Auto-reconnect on stream drop (`-c` to disable) | ✅ |
| Metadata rulefiles (`-w`), external metadata (`-E`), interface bind (`-I`) | ✅ |
| Charset conversion (`--codeset-metadata`/`-id3`/`-filesys`), `--debug`, `--stderr` | ✅ |
| TLS cert check against Mozilla roots (default on; `--no-ssl-verify` to skip) | ✅ |
| **Extras:** `--log` (played.log), `--split-time`/`--split-size`, `-C`/`--config`, `-T` | ✅ |

Verified live against real stations (Polish public radio over http/https, SomaFM,
Radio Paradise FLAC, HLS) on both macOS and a static Linux build on a Synology NAS.

## Dependencies — deliberately minimal

Everything (URL/HTTP/ICY/args/output/relay/HLS) is hand-rolled on `std`. Only two
places take a dependency, where rolling your own would be reckless:

- **TLS:** `rustls` (pure Rust, `ring` backend — no OpenSSL).
- **Audio decode** (for `--wav` + silence): **[symphonia](https://github.com/pdeljanov/Symphonia)**
  (pure Rust) for mp3/aac/ogg-vorbis/flac. One crate replaces the C fork's
  minimp3 + faad2 + libvorbis (three C libraries), with no C build step.

## Install

### Homebrew (macOS / Linuxbrew)

```sh
brew install qbus00/crabripper/crabripper
```

Pours a **prebuilt binary** for your platform — no Rust toolchain, no compile.
Covers Apple Silicon **and Intel** macOS, and `x86_64`/`arm64` Linuxbrew.
Upgrade with `brew upgrade crabripper`.

### Prebuilt binaries

Each [release](https://github.com/qbus00/crabripper/releases) ships binaries for
Apple Silicon macOS, Intel macOS, and static (musl) `x86_64`/`arm64` Linux:

```sh
tar xzf crabripper-macos-arm64.tar.gz   # -> ./crabripper
./crabripper --version
```

## Build

Needs a Rust toolchain (`rustup`). The build script runs fmt + clippy + tests
then builds, with a banner and progress:

```sh
bash scripts/build.sh                 # release, with checks
bash scripts/build.sh --run -- URL -d ./rec -l 60   # build then run
```

### All release architectures (like the C fork)

```sh
bash scripts/build-all.sh             # -> dist/
```

Produces, from a Mac, with no Docker/QEMU:

| Target | Binary |
|---|---|
| Apple Silicon macOS | `crabripper-macos-arm64` |
| Intel macOS | `crabripper-macos-x86_64` |
| x86-64 Linux (static musl) | `crabripper-linux-amd64` |
| aarch64 Linux (static musl) | `crabripper-linux-arm64` |

Both macOS targets build natively with Apple clang (the Intel one cross-compiles
from Apple Silicon). The Linux targets cross-compile via
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) (zig supplies the
linker for the static musl target). Install once: `brew install zig && cargo install cargo-zigbuild`,
plus `rustup target add x86_64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl`.
Each target also gets a `.tar.gz` (inner binary named `crabripper`) for Homebrew.

## Robustness & security

Stream servers (and, for plain http, anyone on the network path) control
everything crabripper reads, so it is built to fail safe:

- **No `unsafe` code**; decoder calls are panic-isolated.
- **Bounded memory and time** from remote input: `icy-metaint` is validated,
  HLS playlists/keys/segments are size- and time-limited, the HLS poll interval
  is clamped so `-l` is always honoured, and silence splitting keeps only a
  short tail in memory.
- **Audio is on disk as it arrives** in every mode, so a crash or `kill`
  loses nothing already received; network errors reconnect instead of ending
  the run.
- **Safe names**: stream titles are sanitized (no `/`, `..`, control chars),
  length-limited to fit filesystems, and never written through symlinks;
  cue sheets, logs, relay headers and the terminal never get raw control
  characters.
- **TLS certificates are verified by default.**
- **Relay**: slow listeners are dropped rather than allowed to stall recording.

## Usage

```sh
crabripper URL [options]
```

Common options (run `-h` for the full streamripper-compatible list):

| Option | Meaning |
|---|---|
| `-d dir` | output directory |
| `-s` | no per-stream subdirectory |
| `-a [file]` | single-file recording (+ cue) |
| `-A` | don't write individual tracks |
| `-l len` | run time: `3600`, `90s`/`12m`/`2h`, or `1:30:00` |
| `-r [[ip:]port]` | relay server (default port 8000) |
| `-u agent` | custom User-Agent |
| `--wav` | decode mp3 tracks to `.wav` |
| `--no-cue` / `-i` | no cue sheet / no ID3 tags |
| `--no-ssl-verify` | skip TLS certificate checks (verification is on by default) |

```sh
# examples
crabripper http://stream4.nadaje.com:9680/radiokrakow-s3 -d ~/rec -l 1h
crabripper https://ice1.somafm.com/indiepop-128-mp3 -r 8000 -u "WinampMPEG/5.0"
crabripper "https://example.com/live/playlist.m3u8" -a show.aac -l 2h
```

## Credits & license

A port of **streamripper** by Jon Clegg and Gregory C. Sharp. Fork changes and
this Rust port by **Jakub Kaprzyk**.

Licensed under the **GNU General Public License, version 2** (GPL-2.0-only), the
same as upstream streamripper.
