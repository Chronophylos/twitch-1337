#!/usr/bin/env python3
"""Check every memories/users/<id>.md against Twitch.

The dreamer used to invent user ids for logins it read out of the transcript,
which left sheets under ids nobody owns and duplicate sheets for one person.
This resolves each sheet's login through Helix and reports where the file name
and the real Twitch id disagree.

Usage:
    scripts/audit-memory-user-ids.py <data-dir>

<data-dir> is the bot's DATA_DIR: it must hold config.toml (for the app
credentials) and memories/users/. Read-only, prints a report.
"""

import json
import pathlib
import re
import sys
import tomllib
import urllib.parse
import urllib.request
from collections import defaultdict


def helix_app_token(client_id: str, client_secret: str) -> str:
    body = urllib.parse.urlencode(
        {
            "client_id": client_id,
            "client_secret": client_secret,
            "grant_type": "client_credentials",
        }
    ).encode()
    req = urllib.request.Request("https://id.twitch.tv/oauth2/token", data=body)
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.load(r)["access_token"]


def helix_users(logins: list[str], client_id: str, token: str) -> dict[str, str]:
    """login (lowercase) -> numeric id, for the logins Twitch still knows."""
    out: dict[str, str] = {}
    for i in range(0, len(logins), 100):  # Helix caps a batch at 100
        q = urllib.parse.urlencode([("login", l) for l in logins[i : i + 100]])
        req = urllib.request.Request(
            f"https://api.twitch.tv/helix/users?{q}",
            headers={"Client-Id": client_id, "Authorization": f"Bearer {token}"},
        )
        with urllib.request.urlopen(req, timeout=15) as r:
            for u in json.load(r)["data"]:
                out[u["login"].lower()] = u["id"]
    return out


def sheet_logins(path: pathlib.Path) -> list[str]:
    """Logins a sheet claims: `username:` frontmatter first, else the body's
    `- Login: …` line. That line sometimes lists several people, which is its
    own bug and worth surfacing."""
    text = path.read_text(encoding="utf-8")
    # `[^\S\n]` and not `\s`: an empty `username:` must not swallow the next
    # frontmatter line as its value.
    fm = re.search(r"^username:[^\S\n]*(\S+)[^\S\n]*$", text, re.M)
    if fm:
        return [fm.group(1).lower()]
    body = re.search(r"^-\s*Login:\s*(.+)$", text, re.M)
    if not body:
        return []
    return [w.lower() for w in re.findall(r"[a-z0-9_]{3,25}", body.group(1), re.I)]


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    data_dir = pathlib.Path(sys.argv[1])
    cfg = tomllib.loads((data_dir / "config.toml").read_text(encoding="utf-8"))
    tw = cfg["twitch"]
    token = helix_app_token(tw["client_id"], tw["client_secret"])

    sheets = sorted((data_dir / "memories" / "users").glob("*.md"))
    claims = {p: sheet_logins(p) for p in sheets}
    all_logins = sorted({l for ls in claims.values() for l in ls})
    real = helix_users(all_logins, tw["client_id"], token)

    by_real: dict[str, list[str]] = defaultdict(list)
    print(f"{len(sheets)} sheets, {len(all_logins)} distinct logins\n")
    for path, logins in claims.items():
        file_id = path.stem
        if not logins:
            print(f"  ?? {path.name}: no login in frontmatter or body")
            continue
        if len(logins) > 1:
            print(f"  !! {path.name}: sheet covers several people: {', '.join(logins)}")
        for login in logins:
            rid = real.get(login)
            if rid is None:
                print(f"  ?? {path.name}: login {login} unknown to Twitch")
            elif rid == file_id:
                by_real[rid].append(path.name)
            else:
                print(f"  XX {path.name}: {login} is really {rid}")
                by_real[rid].append(path.name)

    print()
    for rid, files in sorted(by_real.items()):
        if len(files) > 1:
            print(f"  duplicate: id {rid} is covered by {', '.join(files)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
