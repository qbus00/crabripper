# Shared terminal presentation for crabripper scripts.
# Sourced, not run. Style ported from QRadio/QAudioEditor's scripts/lib/ui.sh.
#
#   ui_banner "rust port" "release  |  aarch64-apple-darwin"
#   ui_section "Checks"
#   ui_step   "Formatting check"
#   ui_run    build "Building (release)" cargo build --release   # spinner, log, report
#   ui_ok     "binary: target/release/crabripper (1.8 MB)"
#   ui_done   "crabripper 0.1.0"
#
# ui_run runs a command with its output going to one reused log in the system
# temp dir, while the terminal shows a single redrawn line: a spinner, elapsed
# time, and the crate currently compiling. On success the line becomes a step
# with its duration and the log is deleted; on failure the tail of the log is
# shown instead.
#
# SR_VERBOSE=1 (or a script's --verbose) streams the command's output instead.
# Off a terminal (piped/redirected) steps print as plain lines, no colour, no
# spinner. SR_UI_NESTED=1 suppresses the banner for a script called by another.
#
# Plain ASCII only: macOS ships bash 3.2, where multi-byte characters break the
# spinner's string slicing.

if [[ -t 1 ]]; then
  UI_BOLD=$'\033[1m'; UI_DIM=$'\033[2m'; UI_GREEN=$'\033[32m'; UI_RED=$'\033[31m'
  UI_YELLOW=$'\033[33m'; UI_CYAN=$'\033[36m'; UI_RESET=$'\033[0m'
  UI_TTY=1
else
  UI_BOLD=""; UI_DIM=""; UI_GREEN=""; UI_RED=""; UI_YELLOW=""; UI_CYAN=""; UI_RESET=""
  UI_TTY=0
fi

# ui_banner SUBTITLE [DETAIL]
ui_banner() {
  if [[ "${SR_UI_NESTED:-0}" == 1 ]]; then return 0; fi
  if [[ "$UI_TTY" != 1 ]]; then
    printf '== crabripper %s ==\n' "$1"
    [[ -n "${2:-}" ]] && printf '%s\n' "$2"
    return 0
  fi
  printf '%s' "$UI_CYAN$UI_BOLD"
  cat <<'ART'
                _          _
  ___ _ __ __ _| |__  _ __(_)_ __  _ __   ___ _ __
 / __| '__/ _` | '_ \| '__| | '_ \| '_ \ / _ \ '__|
| (__| | | (_| | |_) | |  | | |_) | |_) |  __/ |
 \___|_|  \__,_|_.__/|_|  |_| .__/| .__/ \___|_|
                            |_|   |_|
ART
  printf '  %s%s\n' "$1" "$UI_RESET"
  [[ -n "${2:-}" ]] && printf '  %s%s%s\n' "$UI_DIM" "$2" "$UI_RESET"
  printf '\n'
}

ui_section() { printf '\n  %s%s== %s ==%s\n' "$UI_BOLD" "$UI_CYAN" "$1" "$UI_RESET"; }
ui_step()    { printf '  %s>%s %s\n' "$UI_CYAN" "$UI_RESET" "$1"; }
ui_ok()      { printf '  %s+%s %s\n' "$UI_GREEN" "$UI_RESET" "$1"; }
ui_info()    { printf '    %s%s%s\n' "$UI_DIM" "$1" "$UI_RESET"; }
ui_warn()    { printf '  %s!%s %s\n' "$UI_YELLOW" "$UI_RESET" "$1" >&2; }
ui_err()     { printf '  %sx%s %s\n' "$UI_RED" "$UI_RESET" "$1" >&2; }
ui_die()     { ui_err "$1"; exit 1; }

# ui_done "crabripper 0.1.0" ["extra note"]
ui_done() {
  printf '\n  %s%sDone:%s %s' "$UI_GREEN" "$UI_BOLD" "$UI_RESET" "$1"
  [[ -n "${2:-}" ]] && printf ', %s' "$2"
  printf '\n\n'
}

# Printed when a ui_run command fails: the tail of its log.
ui_failure() {
  local label="$1"
  printf '  %sx%s %s %s(failed)%s\n' "$UI_RED" "$UI_RESET" "$label" "$UI_DIM" "$UI_RESET" >&2
  if [[ -f "$UI_LOG" ]]; then
    printf '  %s--- tail of %s ---%s\n' "$UI_DIM" "$UI_LOG" "$UI_RESET" >&2
    tail -n 30 "$UI_LOG" | sed 's/^/    /' >&2
  fi
}

# ui_run NAME LABEL COMMAND [ARGS...]
# NAME names the log ($TMPDIR/sr-NAME.log). Sets UI_SECONDS and UI_LOG; returns
# the command's exit status.
ui_run() {
  local name="$1" label="$2"
  shift 2
  UI_LOG="${TMPDIR:-/tmp}"; UI_LOG="${UI_LOG%/}/sr-$name.log"
  local start=$SECONDS status=0

  if [[ "${SR_VERBOSE:-0}" == 1 ]]; then
    ui_step "$label"
    "$@" || status=$?
    UI_SECONDS=$((SECONDS - start))
    [[ "$status" == 0 ]] || ui_err "$label (failed)"
    return "$status"
  fi

  : >"$UI_LOG"
  "$@" >"$UI_LOG" 2>&1 &
  local pid=$! frames='|/-\' i=0 crate="" latest cols secs shown extra
  if [[ "$UI_TTY" == 1 ]]; then
    while kill -0 "$pid" 2>/dev/null; do
      latest="$(tail -n 20 "$UI_LOG" 2>/dev/null | grep -oE 'Compiling [A-Za-z0-9_.-]+' | tail -n 1 | sed 's/Compiling //' || true)"
      [[ -n "$latest" ]] && crate="$latest"
      cols="$(tput cols 2>/dev/null || echo 80)"
      secs="($((SECONDS - start))s)"
      shown="$label" extra="${crate:+$crate  }"
      if (( 4 + ${#shown} + 1 + ${#extra} + ${#secs} >= cols )); then extra=""; fi
      if (( 4 + ${#shown} + 1 + ${#secs} >= cols )); then
        shown="${shown:0:$((cols > 15 ? cols - 9 - ${#secs} : 1))}..."
      fi
      printf '\r\033[K  %s%s%s %s %s%s%s%s' \
        "$UI_CYAN" "${frames:i%4:1}" "$UI_RESET" "$shown" \
        "$UI_DIM" "$extra" "$secs" "$UI_RESET"
      i=$((i + 1))
      sleep 0.2
    done
    printf '\r\033[K'
  fi
  wait "$pid" || status=$?
  UI_SECONDS=$((SECONDS - start))

  if [[ "$status" == 0 ]]; then
    printf '  %s>%s %s %s(%ds)%s\n' "$UI_CYAN" "$UI_RESET" "$label" "$UI_DIM" "$UI_SECONDS" "$UI_RESET"
    rm -f "$UI_LOG"
  else
    ui_failure "$label"
  fi
  return "$status"
}
