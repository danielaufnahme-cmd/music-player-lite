#!/usr/bin/env bash
# Music Player Lite: one-command setup / update for Linux and macOS
# (Windows: setup.ps1). Safe to run again after every `git pull`.
#
#   1. checks the build tools and libraries and offers to install what is
#      missing (pacman, apt, dnf, zypper or brew; asks before using sudo),
#   2. installs Rust with rustup if there is no recent enough Rust,
#   3. builds the app (cargo build --release),
#   4. installs it for your user (no sudo):
#        <prefix>/bin/music-player-lite            launcher (= launch.sh)
#        <prefix>/lib/music-player-lite/           binary + optional helpers
#        <prefix>/share/applications/music-player-lite.desktop   (Linux)
#      with <prefix> = ~/.local by default,
#   5. optionally adds a systemd user service that runs the server without a
#      window (phone page, device sync, Navidrome) all the time.
#
# Usage: ./setup.sh [options]
#   --check              only check the prerequisites, change nothing
#   --yes                install missing packages without asking
#   --no-deps            never install packages, only print the command
#   --no-desktop-entry   don't add the app-menu entry (Linux)
#   --service            also install + start the systemd user service
#   --remove-service     stop and remove that service again
#   --local-ytdlp        always use a private, self-updating yt-dlp copy
#   --prefix DIR         install below DIR instead of ~/.local
#   --uninstall          remove the installed files (keeps your music + data)
#   --dry-run            print what would be done, change nothing
set -euo pipefail
cd "$(dirname "$0")"
REPO="$(pwd -P)"

APP_NAME="Music Player Lite"
APP_ID="music-player-lite"
MIN_RUST="1.92"

CHECK_ONLY=0
ASSUME_YES=0
NO_DEPS=0
DESKTOP=1
SERVICE=0
REMOVE_SERVICE=0
LOCAL_YTDLP=0
UNINSTALL=0
DRY_RUN=0
PREFIX="${PREFIX:-$HOME/.local}"
while [ $# -gt 0 ]; do
  case "$1" in
    --check) CHECK_ONLY=1 ;;
    --yes | -y) ASSUME_YES=1 ;;
    --no-deps) NO_DEPS=1 ;;
    --no-desktop-entry) DESKTOP=0 ;;
    --service) SERVICE=1 ;;
    --remove-service) REMOVE_SERVICE=1 ;;
    --local-ytdlp) LOCAL_YTDLP=1 ;;
    --prefix)
      [ $# -ge 2 ] || { echo "--prefix needs a directory" >&2; exit 2; }
      PREFIX="$2"
      shift
      ;;
    --prefix=*) PREFIX="${1#--prefix=}" ;;
    --uninstall) UNINSTALL=1 ;;
    --dry-run) DRY_RUN=1 ;;
    -h | --help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
    *) echo "Unknown option: $1 (see ./setup.sh --help)" >&2; exit 2 ;;
  esac
  shift
done
case "$PREFIX" in /*) ;; *) PREFIX="$PWD/$PREFIX" ;; esac

BIN_DIR="$PREFIX/bin"
LIB_DIR="$PREFIX/lib/$APP_ID"
SHARE_DIR="$PREFIX/share"
SYSTEMD_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
SERVICE_FILE="$SYSTEMD_DIR/$APP_ID.service"

say() { printf '==> %s\n' "$*"; }
warn() { printf 'Warning: %s\n' "$*" >&2; }
die() { printf 'Error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
# Run a command, or only print it with --dry-run.
run() {
  if [ "$DRY_RUN" = 1 ]; then
    printf '[dry-run] %s\n' "$*"
  else
    "$@"
  fi
}
ask_yes() {
  [ "$ASSUME_YES" = 1 ] && return 0
  [ -t 0 ] && [ -t 1 ] || return 1
  local answer
  read -r -p "$1 [Y/n] " answer || return 1
  case "$answer" in '' | [yY]*) return 0 ;; *) return 1 ;; esac
}

# --- Which system is this? -------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"
DISTRO=other
OS_RELEASE="${GEONIX_OS_RELEASE:-/etc/os-release}"
if [ "$OS" = "Darwin" ]; then
  DISTRO=macos
elif [ -r "$OS_RELEASE" ]; then
  # shellcheck source=/dev/null
  IDS="$(. "$OS_RELEASE"; echo "${ID:-} ${ID_LIKE:-}")"
  case " $IDS " in
    *" arch "* | *" cachyos "* | *" manjaro "* | *" endeavouros "*) DISTRO=arch ;;
    *" fedora "* | *" rhel "*) DISTRO=fedora ;;
    *" opensuse"* | *" suse "* | *" sles "*) DISTRO=suse ;;
    *" debian "* | *" ubuntu "*) DISTRO=debian ;;
  esac
fi

# --- Uninstall / remove service ----------------------------------------------
remove_service() {
  if [ -f "$SERVICE_FILE" ]; then
    if have systemctl; then
      run systemctl --user disable --now "$APP_ID.service" || true
    fi
    run rm -f "$SERVICE_FILE"
    have systemctl && { run systemctl --user daemon-reload || true; }
    say "Removed the systemd user service"
  else
    say "No systemd user service installed"
  fi
}
if [ "$UNINSTALL" = 1 ]; then
  remove_service
  run rm -f "$BIN_DIR/$APP_ID" "$SHARE_DIR/applications/$APP_ID.desktop" \
    "$SHARE_DIR/icons/hicolor/scalable/apps/$APP_ID.svg"
  run rm -rf "$LIB_DIR"
  say "Uninstalled. Your music and library database were not touched."
  exit 0
fi
if [ "$REMOVE_SERVICE" = 1 ]; then
  remove_service
  exit 0
fi

# --- Prerequisites -----------------------------------------------------------
version_ge() {
  # version_ge 1.93.0 1.92 -> true
  local -a a b
  local i
  IFS=. read -r -a a <<<"$1"
  IFS=. read -r -a b <<<"$2"
  for i in 0 1 2; do
    local x="${a[$i]:-0}" y="${b[$i]:-0}"
    x="${x%%[!0-9]*}"
    y="${y%%[!0-9]*}"
    [ "${x:-0}" -gt "${y:-0}" ] && return 0
    [ "${x:-0}" -lt "${y:-0}" ] && return 1
  done
  return 0
}
has_pc() { have pkg-config && pkg-config --exists "$1" 2>/dev/null; }

# Rust is checked separately (rustup, no sudo needed).
RUST_STATE=ok
if ! have cargo || ! have rustc; then
  RUST_STATE=missing
else
  RUSTC_VERSION="$(rustc --version 2>/dev/null | awk '{print $2}')" || RUSTC_VERSION=""
  if [ -z "$RUSTC_VERSION" ]; then
    RUST_STATE=missing
  elif ! version_ge "$RUSTC_VERSION" "$MIN_RUST"; then
    RUST_STATE=old
  fi
fi

MISSING=()
have curl || MISSING+=(curl)
have ffmpeg || MISSING+=(ffmpeg)
if [ "$DISTRO" = macos ]; then
  xcode-select -p >/dev/null 2>&1 || MISSING+=(xcode-clt)
  have yt-dlp || [ "$LOCAL_YTDLP" = 1 ] || MISSING+=(yt-dlp)
else
  have cc || MISSING+=(cc)
  have pkg-config || MISSING+=(pkg-config)
  has_pc alsa || MISSING+=(alsa)
  has_pc dbus-1 || MISSING+=(dbus)
  has_pc fontconfig || MISSING+=(fontconfig)
  has_pc xkbcommon || MISSING+=(xkbcommon)
  has_pc wayland-client || MISSING+=(wayland)
  has_pc x11 || MISSING+=(x11)
  has_pc egl || has_pc gl || MISSING+=(gl)
  # Debian/Ubuntu ship an old yt-dlp that YouTube breaks quickly: use the
  # private self-updating copy there (and wherever yt-dlp is missing).
  if [ "$DISTRO" = debian ] || [ "$DISTRO" = other ]; then
    have yt-dlp && [ "$DISTRO" != debian ] || LOCAL_YTDLP=1
  else
    have yt-dlp || [ "$LOCAL_YTDLP" = 1 ] || MISSING+=(yt-dlp)
  fi
fi

# Package names per item and system.
package_for() {
  case "$DISTRO:$1" in
    arch:cc) echo "base-devel" ;;
    arch:pkg-config) echo pkgconf ;;
    arch:alsa) echo alsa-lib ;;
    arch:dbus) echo dbus ;;
    arch:xkbcommon) echo libxkbcommon ;;
    arch:x11) echo "libx11 libxcursor libxi libxrandr" ;;
    arch:gl) echo "mesa libglvnd" ;;
    arch:curl | arch:ffmpeg | arch:fontconfig | arch:wayland | arch:yt-dlp) echo "$1" ;;
    debian:cc) echo build-essential ;;
    debian:alsa) echo libasound2-dev ;;
    debian:dbus) echo libdbus-1-dev ;;
    debian:fontconfig) echo libfontconfig1-dev ;;
    debian:xkbcommon) echo libxkbcommon-dev ;;
    debian:wayland) echo libwayland-dev ;;
    debian:x11) echo "libx11-dev libxcursor-dev libxi-dev libxrandr-dev" ;;
    debian:gl) echo "libgl1-mesa-dev libegl1-mesa-dev" ;;
    debian:curl | debian:ffmpeg | debian:pkg-config | debian:yt-dlp) echo "$1" ;;
    fedora:cc) echo "gcc gcc-c++ make" ;;
    fedora:pkg-config) echo pkgconf-pkg-config ;;
    fedora:alsa) echo alsa-lib-devel ;;
    fedora:dbus) echo dbus-devel ;;
    fedora:fontconfig) echo fontconfig-devel ;;
    fedora:xkbcommon) echo libxkbcommon-devel ;;
    fedora:wayland) echo wayland-devel ;;
    fedora:x11) echo "libX11-devel libXcursor-devel libXi-devel libXrandr-devel" ;;
    fedora:gl) echo "mesa-libGL-devel mesa-libEGL-devel" ;;
    fedora:ffmpeg) echo ffmpeg-free ;;
    fedora:curl | fedora:yt-dlp) echo "$1" ;;
    suse:cc) echo "gcc gcc-c++ make" ;;
    suse:pkg-config) echo pkgconf-pkg-config ;;
    suse:alsa) echo alsa-devel ;;
    suse:dbus) echo dbus-1-devel ;;
    suse:fontconfig) echo fontconfig-devel ;;
    suse:xkbcommon) echo libxkbcommon-devel ;;
    suse:wayland) echo wayland-devel ;;
    suse:x11) echo "libX11-devel libXcursor-devel libXi-devel libXrandr-devel" ;;
    suse:gl) echo "Mesa-libGL-devel Mesa-libEGL-devel" ;;
    suse:curl | suse:ffmpeg | suse:yt-dlp) echo "$1" ;;
    macos:curl | macos:ffmpeg | macos:yt-dlp) echo "$1" ;;
  esac
}
install_command() {
  case "$DISTRO" in
    arch) echo "sudo pacman -S --needed $1" ;;
    debian) echo "sudo apt-get install -y $1" ;;
    fedora) echo "sudo dnf install -y $1" ;;
    suse) echo "sudo zypper install -y $1" ;;
    macos) echo "brew install $1" ;;
  esac
}

PKGS=""
for item in "${MISSING[@]+"${MISSING[@]}"}"; do
  p="$(package_for "$item")"
  [ -n "$p" ] && PKGS="$PKGS $p"
done
PKGS="${PKGS# }"

echo "System: $OS $ARCH ($DISTRO)"
if [ "$RUST_STATE" = ok ]; then
  echo "Rust:   $(rustc --version)"
else
  echo "Rust:   $RUST_STATE (need $MIN_RUST or newer)"
fi
if [ "${#MISSING[@]}" -gt 0 ]; then
  echo "Missing: ${MISSING[*]}"
  [ -n "$PKGS" ] && [ "$DISTRO" != other ] && echo "Install: $(install_command "$PKGS")"
else
  echo "Libraries and tools: OK"
fi
[ "$LOCAL_YTDLP" = 1 ] && echo "yt-dlp: a private copy will be installed to $LIB_DIR (updates itself weekly)"

if [ "$CHECK_ONLY" = 1 ]; then
  if [ "$RUST_STATE" = ok ] && [ "${#MISSING[@]}" -eq 0 ]; then exit 0; fi
  exit 1
fi

# --- Install what is missing ---------------------------------------------------
if [ "${#MISSING[@]}" -gt 0 ]; then
  if [ "$DISTRO" = other ]; then
    die "please install these with your package manager (development packages for the libraries): ${MISSING[*]}"
  fi
  if [ "$DISTRO" = macos ]; then
    case " ${MISSING[*]} " in
      *" xcode-clt "*)
        say "Installing the Xcode command line tools (a window opens; run ./setup.sh again when it is done)"
        run xcode-select --install || true
        exit 1
        ;;
    esac
    have brew || die "Homebrew is needed: see https://brew.sh, then run ./setup.sh again"
  fi
  CMD="$(install_command "$PKGS")"
  if [ "$NO_DEPS" = 1 ]; then
    die "missing packages. Install them with: $CMD"
  fi
  if ask_yes "Install the missing packages now ($CMD)?"; then
    say "Installing: $PKGS"
    case "$DISTRO" in
      debian) run sudo apt-get update ;;
    esac
    # shellcheck disable=SC2086
    run $CMD
  else
    die "missing packages. Install them with: $CMD"
  fi
fi

if [ "$RUST_STATE" = missing ]; then
  if [ "$DISTRO" = arch ] && ! have rustup; then
    RUSTUP_CMD="sudo pacman -S --needed rustup"
    if [ "$NO_DEPS" = 0 ] && ask_yes "Install Rust with rustup ($RUSTUP_CMD)?"; then
      run sudo pacman -S --needed rustup
    else
      die "Rust is missing. Install it with: $RUSTUP_CMD && rustup default stable"
    fi
  fi
  if have rustup; then
    say "Installing the stable Rust toolchain (rustup)"
    run rustup default stable
  elif ask_yes "Install Rust with rustup from https://rustup.rs (for your user, no sudo)?"; then
    say "Installing Rust (rustup)"
    if [ "$DRY_RUN" = 1 ]; then
      echo "[dry-run] curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"
    else
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    fi
  else
    die "Rust is missing. Install it from https://rustup.rs and run ./setup.sh again"
  fi
  # shellcheck disable=SC1091
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
elif [ "$RUST_STATE" = old ]; then
  if have rustup; then
    say "Updating Rust (rustup update stable)"
    run rustup update stable
  else
    die "your Rust ($(rustc --version)) is older than $MIN_RUST, probably from the distro packages. Install Rust from https://rustup.rs instead and run ./setup.sh again"
  fi
fi

# --- Build ---------------------------------------------------------------------
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
say "Building $APP_NAME (cargo build --release; the first build takes a few minutes)"
run cargo build --release --locked
BUILT="$TARGET_DIR/release/geonix-lite"
[ "$DRY_RUN" = 1 ] || [ -x "$BUILT" ] || die "build finished but $BUILT is missing"

# --- Install -------------------------------------------------------------------
# Copy to a temp name, then rename: a running copy keeps working.
install_file() {
  local mode="$1" src="$2" dest="$3"
  run mkdir -p "$(dirname "$dest")"
  run cp "$src" "$dest.new"
  run chmod "$mode" "$dest.new"
  run mv -f "$dest.new" "$dest"
}

say "Installing to $PREFIX"
install_file 755 "$BUILT" "$LIB_DIR/geonix-lite"
install_file 755 "$REPO/launch.sh" "$BIN_DIR/$APP_ID"
install_file 755 "$REPO/tools/after_downloads.sh" "$LIB_DIR/after_downloads.sh"
install_file 644 "$REPO/tools/sync_playlists.py" "$LIB_DIR/sync_playlists.py"
install_file 644 "$REPO/tools/navidrome_rescan.py" "$LIB_DIR/navidrome_rescan.py"

if [ "$LOCAL_YTDLP" = 1 ]; then
  case "$OS:$ARCH" in
    Linux:x86_64) YTDLP_ASSET=yt-dlp_linux ;;
    Linux:aarch64 | Linux:arm64) YTDLP_ASSET=yt-dlp_linux_aarch64 ;;
    Darwin:*) YTDLP_ASSET=yt-dlp_macos ;;
    *) YTDLP_ASSET=yt-dlp ;;
  esac
  if [ -x "$LIB_DIR/yt-dlp" ]; then
    say "Updating the private yt-dlp"
    run "$LIB_DIR/yt-dlp" -U || warn "yt-dlp update failed (it retries weekly)"
  else
    say "Downloading yt-dlp ($YTDLP_ASSET)"
    run mkdir -p "$LIB_DIR"
    run curl -fL --proto '=https' -o "$LIB_DIR/yt-dlp.new" \
      "https://github.com/yt-dlp/yt-dlp/releases/latest/download/$YTDLP_ASSET"
    run chmod 755 "$LIB_DIR/yt-dlp.new"
    run mv -f "$LIB_DIR/yt-dlp.new" "$LIB_DIR/yt-dlp"
  fi
fi

if [ "$OS" = "Linux" ]; then
  install_file 644 "$REPO/crates/server/web/icon.svg" "$SHARE_DIR/icons/hicolor/scalable/apps/$APP_ID.svg"
  DESKTOP_FILE="$SHARE_DIR/applications/$APP_ID.desktop"
  if [ "$DESKTOP" = 1 ]; then
    say "Menu entry: $DESKTOP_FILE"
    if [ "$DRY_RUN" = 0 ]; then
      mkdir -p "$(dirname "$DESKTOP_FILE")"
      cat >"$DESKTOP_FILE.new" <<EOF
[Desktop Entry]
Type=Application
Name=$APP_NAME
GenericName=Music Player
Comment=Search, download and play music
Exec="$BIN_DIR/$APP_ID"
Icon=$APP_ID
Terminal=false
Categories=AudioVideo;Audio;Player;
Keywords=music;player;youtube;playlist;
StartupWMClass=$APP_ID
EOF
      mv -f "$DESKTOP_FILE.new" "$DESKTOP_FILE"
      have update-desktop-database && update-desktop-database -q "$(dirname "$DESKTOP_FILE")" 2>/dev/null || true
    fi
  fi
fi

# --- systemd user service (headless server) -------------------------------------
if [ "$SERVICE" = 1 ]; then
  if [ "$OS" != "Linux" ] || ! have systemctl; then
    warn "--service needs Linux with systemd; skipped. Run '$BIN_DIR/$APP_ID --headless' instead."
  else
    say "systemd user service: $SERVICE_FILE"
    if [ "$DRY_RUN" = 0 ]; then
      mkdir -p "$SYSTEMD_DIR"
      cat >"$SERVICE_FILE" <<EOF
[Unit]
Description=$APP_NAME server (phone page, device sync)
After=network-online.target

[Service]
ExecStart="$BIN_DIR/$APP_ID" --headless
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
    fi
    run systemctl --user daemon-reload
    run systemctl --user enable --now "$APP_ID.service"
    run systemctl --user restart "$APP_ID.service"
  fi
elif [ -f "$SERVICE_FILE" ]; then
  # Already installed earlier: restart it so it runs the new build.
  if have systemctl && systemctl --user is-active --quiet "$APP_ID.service" 2>/dev/null; then
    say "Restarting the systemd user service with the new build"
    run systemctl --user restart "$APP_ID.service"
  fi
fi

echo
echo "Done."
case ":$PATH:" in
  *":$BIN_DIR:"*) echo "Start it from the app menu or with: $APP_ID" ;;
  *)
    echo "Start it from the app menu or with: $BIN_DIR/$APP_ID"
    echo "($BIN_DIR is not on your PATH.)"
    ;;
esac
echo "From this folder you can also run: ./launch.sh"
