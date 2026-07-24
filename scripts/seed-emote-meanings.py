# /// script
# requires-python = ">=3.11"
# ///
"""DEPRECATED(emote-meanings): one-shot TOML→SQLite seed for emote_meanings.

MIGRATION(emote-meanings): import baked 7tv_emotes.toml into emotes.db — remove when bake file retired.

Prerequisite: start the bot (or otherwise open emotes.db via EmoteTracker) once so schema
v3 migration runs. This script does not call migrate(); seeding against an unmigrated DB
will fail.

Maps TOML `meaning` → SQL `appearance`; leaves SQL `meaning` NULL so curated values survive re-runs.

Fail-closed: exits 1 when any name is unresolved or collides, but successful upserts are
committed before that check. Re-run is idempotent (appearance/usage/avoid refresh; curated
`meaning` preserved via COALESCE).
"""

from __future__ import annotations

import argparse
import json
import sqlite3
import sys
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_TOML = REPO_ROOT / "crates" / "data" / "assets" / "emotes" / "7tv_emotes.toml"
DEFAULT_BASE_URL = "https://7tv.io/v3"
DB_FILENAME = "emotes.db"
# MIGRATION(emote-meanings): accept legacy DB filename when emotes.db absent — remove when bake file retired
LEGACY_DB_FILENAME = "emote_tracker.sqlite"


def resolve_db_path(db_arg: Path) -> Path:
    if db_arg.is_dir():
        preferred = db_arg / DB_FILENAME
        legacy = db_arg / LEGACY_DB_FILENAME
        if preferred.is_file():
            return preferred
        if legacy.is_file():
            return legacy
        return preferred
    return db_arg


def load_toml_entries(toml_path: Path) -> list[dict[str, str | None]]:
    with open(toml_path, "rb") as f:
        data = tomllib.load(f)
    rows: list[dict[str, str | None]] = []
    for emote in data.get("emotes", []):
        name = emote.get("name")
        if not isinstance(name, str) or not name.strip():
            continue
        meaning = emote.get("meaning")
        appearance = meaning.strip() if isinstance(meaning, str) and meaning.strip() else None
        if appearance is None:
            print(f"skip: {name!r} has no TOML meaning/appearance", file=sys.stderr)
            continue
        usage = emote.get("usage")
        avoid = emote.get("avoid")
        rows.append(
            {
                "name": name.strip(),
                "appearance": appearance,
                "usage": usage.strip()
                if isinstance(usage, str) and usage.strip()
                else None,
                "avoid": avoid.strip()
                if isinstance(avoid, str) and avoid.strip()
                else None,
            }
        )
    return rows


def pick_name_id(
    candidates: list[tuple[str, int, int]],
) -> tuple[str | None, str | None]:
    """Return (emote_id, error). candidates: (emote_id, active, updated_at)."""
    if not candidates:
        return None, None
    active = [c for c in candidates if c[1] == 1]
    pool = active if active else candidates
    if active and len(active) > 1:
        ids = sorted({c[0] for c in active})
        if len(ids) > 1:
            return None, f"multiple active emotes: {', '.join(ids)}"
    best_updated = max(c[2] for c in pool)
    best = [c for c in pool if c[2] == best_updated]
    ids = sorted({c[0] for c in best})
    if len(ids) > 1:
        return None, f"ambiguous updated_at={best_updated}: {', '.join(ids)}"
    return ids[0], None


def build_tracker_name_map(
    conn: sqlite3.Connection,
) -> tuple[dict[str, str], dict[str, str]]:
    resolved: dict[str, str] = {}
    collisions: dict[str, str] = {}
    by_name: dict[str, list[tuple[str, int, int]]] = {}
    for emote_id, name, active, updated_at in conn.execute(
        "SELECT emote_id, name, active, updated_at FROM emotes"
    ):
        by_name.setdefault(name, []).append((emote_id, int(active), int(updated_at)))
    for name, candidates in sorted(by_name.items()):
        emote_id, err = pick_name_id(candidates)
        if err:
            collisions[name] = err
        elif emote_id:
            resolved[name] = emote_id
    return resolved, collisions


def fetch_global_name_map(
    base_url: str,
) -> tuple[dict[str, str], dict[str, str]]:
    url = f"{base_url.rstrip('/')}/emote-sets/global"
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            payload = json.load(resp)
    except urllib.error.URLError as exc:
        raise SystemExit(f"failed to fetch global emote set from {url}: {exc}") from exc

    by_name: dict[str, list[str]] = {}
    for emote in payload.get("emotes", []):
        name = emote.get("name")
        emote_id = emote.get("id")
        if not isinstance(name, str) or not name.strip():
            continue
        if not isinstance(emote_id, str) or not emote_id.strip():
            continue
        by_name.setdefault(name.strip(), []).append(emote_id.strip())

    resolved: dict[str, str] = {}
    collisions: dict[str, str] = {}
    for name, ids in by_name.items():
        unique = sorted(set(ids))
        if len(unique) == 1:
            resolved[name] = unique[0]
        else:
            collisions[name] = f"multiple global emotes: {', '.join(unique)}"
    return resolved, collisions


def upsert_meaning(
    conn: sqlite3.Connection,
    emote_id: str,
    appearance: str,
    usage: str | None,
    avoid: str | None,
    updated_at: int,
) -> None:
    conn.execute(
        """
        INSERT INTO emote_meanings (emote_id, appearance, meaning, usage, avoid, updated_at)
        VALUES (?, ?, NULL, ?, ?, ?)
        ON CONFLICT(emote_id) DO UPDATE SET
          appearance = excluded.appearance,
          meaning = COALESCE(excluded.meaning, emote_meanings.meaning),
          usage = excluded.usage,
          avoid = excluded.avoid,
          updated_at = excluded.updated_at
        """,
        (emote_id, appearance, usage, avoid, updated_at),
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Seed emote_meanings from baked 7tv_emotes.toml into emotes.db"
    )
    parser.add_argument(
        "--db",
        required=True,
        type=Path,
        help="Path to emotes.db or a data directory containing it",
    )
    parser.add_argument(
        "--toml",
        type=Path,
        default=DEFAULT_TOML,
        help=f"Glossary TOML (default: {DEFAULT_TOML})",
    )
    parser.add_argument(
        "--base-url",
        default=DEFAULT_BASE_URL,
        help=f"7TV REST base URL (default: {DEFAULT_BASE_URL})",
    )
    args = parser.parse_args()

    db_path = resolve_db_path(args.db)
    if not db_path.is_file():
        print(f"error: database not found: {db_path}", file=sys.stderr)
        return 1

    if not args.toml.is_file():
        print(f"error: TOML not found: {args.toml}", file=sys.stderr)
        return 1

    entries = load_toml_entries(args.toml)
    conn = sqlite3.connect(db_path)
    try:
        conn.execute("PRAGMA foreign_keys = ON")
        tracker_map, tracker_collisions = build_tracker_name_map(conn)
        global_map, global_collisions = fetch_global_name_map(args.base_url)

        resolved_tracker = 0
        resolved_global = 0
        upserted = 0
        unresolved: list[str] = []
        collisions: dict[str, str] = {}
        updated_at = int(time.time() * 1000)

        for entry in entries:
            name = entry["name"]
            if name in tracker_collisions:
                collisions[name] = tracker_collisions[name]
                continue
            emote_id = tracker_map.get(name)
            source = "tracker"
            if emote_id is None:
                if name in global_collisions:
                    collisions[name] = global_collisions[name]
                    continue
                emote_id = global_map.get(name)
                source = "global"
            if emote_id is None:
                unresolved.append(name)
                continue
            if source == "tracker":
                resolved_tracker += 1
            else:
                resolved_global += 1
            upsert_meaning(
                conn,
                emote_id,
                entry["appearance"],
                entry["usage"],
                entry["avoid"],
                updated_at,
            )
            upserted += 1

        conn.commit()

        print(f"toml_entries: {len(entries)}")
        print(f"resolved_tracker: {resolved_tracker}")
        print(f"resolved_global: {resolved_global}")
        print(f"upserted: {upserted}")
        print(f"unresolved: {len(unresolved)}")
        print(f"collisions: {len(collisions)}")
        if unresolved:
            print("unresolved names:", file=sys.stderr)
            for name in unresolved[:20]:
                print(f"  - {name}", file=sys.stderr)
            if len(unresolved) > 20:
                print(f"  ... and {len(unresolved) - 20} more", file=sys.stderr)
        if collisions:
            print("name collisions:", file=sys.stderr)
            for name, detail in list(collisions.items())[:20]:
                print(f"  - {name}: {detail}", file=sys.stderr)
            if len(collisions) > 20:
                print(f"  ... and {len(collisions) - 20} more", file=sys.stderr)

        if unresolved or collisions:
            return 1
        return 0
    finally:
        conn.close()


if __name__ == "__main__":
    raise SystemExit(main())
