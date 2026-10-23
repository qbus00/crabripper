# streamripper-rs

A work-in-progress **Rust port of [streamripper](https://github.com/qbus00/streamripper)** —
records shoutcast/icecast radio streams to disk, split into per-track files.

This is an independent sibling to the C fork; it does not share its git history.

## Status

Early, but it genuinely records. The first slice covers the core path:

- **HTTP and HTTPS** stream connections (TLS via `native-tls`; certificate
  verification is off by default, matching the C fork).
- **ICY inline metadata** (`Icy-MetaData: 1`): reads `icy-metaint`, `icy-name`,
  `content-type`, de-interleaves the metadata blocks.
- **Track splitting** driven by `StreamTitle` changes — one file per song, named
  from the metadata.
- Handles both `HTTP/1.x` and shoutcast's `ICY 200 OK` status lines.
- Extension chosen from content-type (`mp3`/`aac`/`ogg`/`flac`).

Verified live against real stations (Radio Kraków over HTTP, SomaFM over HTTPS —
the latter rolled cleanly from the buffering track into a correctly-named song
file).

## Build

Needs a Rust toolchain (`rustup`). The build script does the ceremony — toolchain
check, `rustfmt` check, `clippy -D warnings`, tests, then the build — and reports
the binary:

```sh
bash scripts/build.sh            # release build, with checks + tests
bash scripts/build.sh --debug    # debug build
bash scripts/build.sh --fast     # skip checks/tests
bash scripts/build.sh --run -- <url> -d ./rec -l 60   # build then run
```

Or plain cargo:

```sh
cargo build --release
```

## Usage

```sh
streamripper-rs <URL> [options]

  -d <dir>    output directory (default: current dir)
  -l <secs>   stop after N seconds (default: run until stopped)
  -u <agent>  HTTP User-Agent
  -m <secs>   connect/read timeout (default: 30)

# example
streamripper-rs http://stream4.nadaje.com:9680/radiokrakow-s3 -d ./rec -l 60
```

## Roadmap

Ordered roughly by priority:

- **Follow HTTP redirects** (301/302/307) — some CDNs (e.g. Radio 357) redirect.
- **Switch TLS to `rustls`** (pure Rust) so Linux builds are fully static with no
  OpenSSL — enables trivial cross-compilation from macOS.
- **HLS (`.m3u8`)** playlist polling + segment concatenation.
- **Silence-based splitpoint** refinement (decode mp3/aac to find a quiet point).
- **FLAC** capture (native + Ogg-FLAC), **cue sheets**, **`--wav`** decode.
- **Relay server**, single-file (`-a`) mode, config flags parity with the C CLI.

## License

GPL-2.0-only, matching upstream streamripper.
Fork/port by Jakub Kaprzyk.
