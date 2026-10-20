#!/usr/bin/env python3
"""Ask the local Navidrome server to rescan the music folder now.

Reads the login from ~/.config/navidrome/LOGIN.txt (or NAVIDROME_LOGIN_FILE)
and uses Subsonic token auth, so the password is never sent or printed in
plain text. Exits quietly with status 0 if Navidrome isn't set up.
"""

from __future__ import annotations

import hashlib
import json
import os
import secrets
import sys
import urllib.parse
import urllib.request
from pathlib import Path

URL = os.environ.get("NAVIDROME_URL", "http://127.0.0.1:4533")


def read_login(path: Path) -> tuple[str, str] | None:
    try:
        fields = dict(
            line.split(":", 1) for line in path.read_text(encoding="utf-8").splitlines() if ":" in line
        )
    except OSError:
        return None
    user, password = fields.get("Username", "").strip(), fields.get("Password", "").strip()
    return (user, password) if user and password else None


def auth_query(user: str, password: str) -> str:
    salt = secrets.token_hex(6)
    token = hashlib.md5((password + salt).encode()).hexdigest()  # Subsonic API requires md5 here
    return urllib.parse.urlencode({"u": user, "t": token, "s": salt, "v": "1.16.1", "c": "music-player-lite", "f": "json"})


def main() -> int:
    login_file = Path(os.environ.get("NAVIDROME_LOGIN_FILE", Path.home() / ".config/navidrome/LOGIN.txt"))
    login = read_login(login_file)
    if not login:
        return 0
    try:
        with urllib.request.urlopen(f"{URL}/rest/startScan?{auth_query(*login)}", timeout=10) as res:
            status = json.load(res).get("subsonic-response", {}).get("status")
    except (OSError, ValueError) as exc:
        print(f"navidrome rescan skipped: {exc.__class__.__name__}", file=sys.stderr)
        return 0
    print(f"navidrome rescan: {status}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
