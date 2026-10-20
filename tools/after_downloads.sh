#!/usr/bin/env bash
# Run by the server (GEONIX_AFTER_DOWNLOADS) shortly after downloads or
# playlist imports finish: export playlists for Navidrome, then ask Navidrome
# to rescan so the phone app sees new songs right away.
cd "$(dirname "$0")" || exit 1
python3 sync_playlists.py
python3 navidrome_rescan.py
