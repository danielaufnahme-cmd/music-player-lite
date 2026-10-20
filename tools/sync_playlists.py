#!/usr/bin/env python3
"""Export Music Player Lite playlists as .m3u8 files so Navidrome (and the phone) see them.

Writes one file per playlist into <music dir>/Playlists/, with paths relative
to that folder. Only downloaded songs are included; the Discover Mix is
skipped because its songs mostly aren't downloaded. Files are only rewritten
when their content changes, and playlists deleted in the app are removed.

Usage: sync_playlists.py [--db PATH] [--music-dir PATH]
Defaults match the app: GEONIX_DATA_DIR / GEONIX_MUSIC_DIR from the
environment, else from ~/.config/geonix-lite/geonix.env, else the app's own
defaults (~/.local/share/geonix-lite, macOS ~/Library/Application
Support/geonix-lite, Windows %LOCALAPPDATA%/geonix-lite; ~/Music/Geonix-lite).
"""

from __future__ import annotations

import argparse
import os
import re
import sqlite3
import sys
from pathlib import Path

MARKER = "#GEONIX-SYNC"


def config_path() -> Path:
    if os.environ.get("GEONIX_CONFIG"):
        return Path(os.environ["GEONIX_CONFIG"])
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "geonix-lite" / "geonix.env"


def default_data_dir() -> Path:
    """Same as the app (Rust `dirs::data_local_dir()` + "geonix-lite")."""
    home = Path.home()
    if sys.platform == "darwin":
        return home / "Library" / "Application Support" / "geonix-lite"
    if os.name == "nt":
        return Path(os.environ.get("LOCALAPPDATA") or home / "AppData" / "Local") / "geonix-lite"
    return Path(os.environ.get("XDG_DATA_HOME") or home / ".local" / "share") / "geonix-lite"


def load_config(path: Path, environ=os.environ) -> dict[str, str]:
    """GEONIX_* settings from the optional KEY=VALUE file (same rules as
    launch.sh), with non-empty environment variables winning."""
    settings: dict[str, str] = {}
    home = str(Path.home())
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        lines = []
    for line in lines:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        line = line.removeprefix("export ")
        key, sep, value = line.partition("=")
        if not sep or not re.fullmatch(r"GEONIX_[A-Z0-9_]*", key):
            continue
        if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        if value == "~" or value.startswith("~/"):
            value = home + value[1:]
        settings[key] = value.replace("${HOME}", home).replace("$HOME", home)
    settings.update({k: v for k, v in environ.items() if k.startswith("GEONIX_") and v})
    return settings


def safe_filename(name: str) -> str:
    cleaned = re.sub(r'[\\/:*?"<>|\x00-\x1f]', "_", name).strip(" .")
    return cleaned or "Playlist"


def read_playlists(db_path: Path) -> list[tuple[str, list[str]]]:
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        playlists = conn.execute(
            "SELECT id, name FROM playlists WHERE kind != 'discover' ORDER BY id"
        ).fetchall()
        out = []
        for pid, name in playlists:
            rows = conn.execute(
                """SELECT t.file_path FROM playlist_tracks pt
                   JOIN tracks t ON t.video_id = pt.video_id
                   WHERE pt.playlist_id = ? AND t.file_path IS NOT NULL
                   ORDER BY pt.position""",
                (pid,),
            ).fetchall()
            out.append((name, [r[0] for r in rows]))
        return out
    finally:
        conn.close()


def render(name: str, files: list[str], playlist_dir: Path) -> str:
    lines = ["#EXTM3U", MARKER, f"#PLAYLIST:{name}"]
    for f in files:
        lines.append(os.path.relpath(f, playlist_dir))
    return "\n".join(lines) + "\n"


def sync(db_path: Path, music_dir: Path) -> list[str]:
    playlist_dir = music_dir / "Playlists"
    playlist_dir.mkdir(parents=True, exist_ok=True)
    changes: list[str] = []
    wanted: set[str] = set()
    used: set[str] = set()

    for name, files in read_playlists(db_path):
        base = safe_filename(name)
        fname, n = f"{base}.m3u8", 2
        while fname.lower() in used:  # two playlists with the same name
            fname, n = f"{base} ({n}).m3u8", n + 1
        used.add(fname.lower())
        wanted.add(fname)
        path = playlist_dir / fname
        content = render(name, files, playlist_dir)
        if not path.exists() or path.read_text(encoding="utf-8") != content:
            tmp = path.with_suffix(".m3u8.tmp")
            tmp.write_text(content, encoding="utf-8")
            tmp.replace(path)
            changes.append(f"wrote {fname} ({len(files)} songs)")

    # Remove files this script created for playlists that no longer exist.
    for path in playlist_dir.glob("*.m3u8"):
        if path.name in wanted:
            continue
        try:
            head = path.read_text(encoding="utf-8").splitlines()[:2]
        except OSError:
            continue
        if MARKER in head:
            path.unlink()
            changes.append(f"removed {path.name}")
    return changes


def main() -> None:
    home = Path.home()
    settings = load_config(config_path())
    data_dir = Path(settings.get("GEONIX_DATA_DIR") or default_data_dir())
    music_dir = Path(settings.get("GEONIX_MUSIC_DIR") or home / "Music" / "Geonix-lite")
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--db", type=Path, default=data_dir / "library.db")
    parser.add_argument("--music-dir", type=Path, default=music_dir)
    args = parser.parse_args()
    if not args.db.exists():
        print(f"no database at {args.db}; nothing to sync")
        return
    for line in sync(args.db, args.music_dir):
        print(line)


if __name__ == "__main__":
    main()
