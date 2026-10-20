#!/usr/bin/env bash
# Start Music Player Lite (Linux and macOS; Windows: launch.ps1).
#
# Works both from the repository (after `cargo build --release` or ./setup.sh)
# and as the installed launcher ~/.local/bin/music-player-lite.
# All arguments are passed on to the binary, e.g. `launch.sh --headless`
# runs only the server (phone page, device sync) without a window.
#
# Settings: real environment variables win, then the optional config file
#   ${GEONIX_CONFIG:-${XDG_CONFIG_HOME:-~/.config}/geonix-lite/geonix.env}
# (KEY=VALUE lines, # comments, ~ and $HOME expanded; only GEONIX_* keys),
# then the app's own defaults (port 7902, sync port 7903,
# data ~/.local/share/geonix-lite, music ~/Music/Geonix-lite).
set -uo pipefail

# --- Find ourselves (follow symlinks; macOS has no `readlink -f`) ----------
SELF="$0"
while [ -L "$SELF" ]; do
  LINK="$(readlink "$SELF")"
  case "$LINK" in
    /*) SELF="$LINK" ;;
    *) SELF="$(dirname "$SELF")/$LINK" ;;
  esac
done
HERE="$(cd "$(dirname "$SELF")" && pwd -P)"

# --- Optional config file ----------------------------------------------------
load_config() {
  local file="${GEONIX_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/geonix-lite/geonix.env}"
  [ -f "$file" ] || return 0
  local line key value
  while IFS= read -r line || [ -n "$line" ]; do
    line="${line%$'\r'}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    case "$line" in '' | '#'*) continue ;; esac
    line="${line#export }"
    case "$line" in *=*) ;; *) continue ;; esac
    key="${line%%=*}"
    value="${line#*=}"
    case "$key" in GEONIX_*) ;; *) continue ;; esac
    case "$key" in *[!A-Z0-9_]*) continue ;; esac
    case "$value" in
      \"*\") value="${value#\"}"; value="${value%\"}" ;;
      \'*\') value="${value#\'}"; value="${value%\'}" ;;
    esac
    # shellcheck disable=SC2088
    case "$value" in '~' | '~/'*) value="$HOME${value#\~}" ;; esac
    value="${value//\$\{HOME\}/$HOME}"
    value="${value//\$HOME/$HOME}"
    [ -n "${!key:-}" ] && continue
    export "$key=$value"
  done <"$file"
}
load_config

# --- Find the binary ---------------------------------------------------------
# GEONIX_LITE_BIN, then the installed layout (<prefix>/bin/music-player-lite
# -> <prefix>/lib/music-player-lite/geonix-lite), then a repo build.
BIN=""
LIBDIR=""
for candidate in \
  "${GEONIX_LITE_BIN:-}" \
  "$HERE/../lib/music-player-lite/geonix-lite" \
  "$HERE/geonix-lite" \
  "${CARGO_TARGET_DIR:-$HERE/target}/release/geonix-lite"; do
  if [ -n "$candidate" ] && [ -x "$candidate" ] && [ -f "$candidate" ]; then
    BIN="$candidate"
    LIBDIR="$(cd "$(dirname "$candidate")" && pwd -P)"
    break
  fi
done
if [ -z "$BIN" ]; then
  echo "Music Player Lite is not built yet. Run ./setup.sh (or: cargo build --release)." >&2
  command -v notify-send >/dev/null 2>&1 &&
    notify-send -a "Music Player Lite" -i dialog-error "Music Player Lite" "Not built yet: run ./setup.sh" 2>/dev/null
  exit 1
fi

# Window/app id: matches the installed music-player-lite.desktop entry.
export GEONIX_APP_ID="${GEONIX_APP_ID:-music-player-lite}"

# A private yt-dlp copy installed by setup.sh (next to the binary) wins over
# an older one from the system, and is updated in the background once a week.
if [ -z "${GEONIX_YTDLP:-}" ] && [ -x "$LIBDIR/yt-dlp" ]; then
  export GEONIX_YTDLP="$LIBDIR/yt-dlp"
  STAMP="$LIBDIR/.yt-dlp-updated"
  if [ -w "$LIBDIR/yt-dlp" ] && [ -z "$(find "$STAMP" -mtime -7 2>/dev/null)" ]; then
    ("$LIBDIR/yt-dlp" -U >/dev/null 2>&1 && touch "$STAMP") &
  fi
fi

# Started from the app menu (no terminal, no arguments): keep a log file.
# Under systemd (INVOCATION_ID set) the journal has the output.
if [ ! -t 1 ] && [ $# -eq 0 ] && [ -z "${INVOCATION_ID:-}" ]; then
  if [ "$(uname -s)" = "Darwin" ]; then
    DATA="${GEONIX_DATA_DIR:-$HOME/Library/Application Support/geonix-lite}"
  else
    DATA="${GEONIX_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/geonix-lite}"
  fi
  if mkdir -p "$DATA" 2>/dev/null; then
    exec >>"$DATA/launch.log" 2>&1
    echo "=== $(date '+%F %T') start $BIN $*"
  fi
fi

exec "$BIN" "$@"
