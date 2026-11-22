# `/commands.json`

Public, unauthenticated list of the bot's chat commands, meant for
autocomplete in chat clients. It is served by the dashboard at
`<public_url>/commands.json` and generated live from the command registry,
so it always matches what the bot answers to.

```json
{
  "version": 1,
  "bot": "twitch-1337",
  "channel": "somechannel",
  "lang": "de",
  "commands": [
    {
      "name": "!ai",
      "aliases": ["@grok"],
      "kind": "command",
      "permission": "everyone",
      "description": "Frag Aurora, mit @grok antwortet Grok",
      "usage": "!ai <anweisung>"
    },
    {
      "name": "!kaffee",
      "aliases": [],
      "kind": "ping",
      "permission": "everyone",
      "description": "Pingt die kaffee-Gruppe",
      "usage": "!kaffee"
    }
  ]
}
```

## Fields

| Field | Meaning |
|---|---|
| `version` | Format version. Bumped only on breaking changes. |
| `bot`, `channel` | Which bot and which channel the list belongs to. |
| `lang` | Language of `description`. |
| `name` | Trigger including its prefix. |
| `aliases` | Other triggers for the same command, including their prefix (`@grok`). |
| `kind` | `command`, or `ping` for a user-created ping group. |
| `permission` | `everyone`, or `moderator` (broadcaster or mod badge). |
| `description` | One line, German. |
| `usage` | Hint shown next to the trigger. `<x>` is required, `[x]` optional. |

## Behaviour

- Commands that are off right now are left out (aviation without a client,
  `!spoiler` without a profile). Suspended commands stay listed.
- Pings never expose their members or template. While pings are
  members-only (`pings.public = false`), no pings are listed at all.
- A ping shadowed by a built-in command of the same name is left out,
  since it can never fire.
- Matching in chat is case-insensitive for some triggers; clients should
  compare case-insensitively too.
- `Cache-Control: public, max-age=300`. A `503` means the bot is still
  starting.
- No CORS headers. Native apps don't need them; a browser client would.

## `/commands` and `!help`

`<public_url>/commands` is the same chat-trigger list as a public HTML page
for viewers, German like the descriptions: built-in commands first, then pings,
each with its trigger, aliases, usage, description and a Mod marker for
`permission = moderator`. It reads the same catalog as `/commands.json`, so
those sections hide exactly what the JSON hides, and it answers with the same
`503` while the bot starts and the same `Cache-Control`. No session needed.

A separate **Aurora-Werkzeuge** section follows the chat Commands and Pings.
It documents each enabled Viewer-Facing Tool once with a friendly name, a
short description and a concrete `!ai …` example, including the Bash sandbox
and the two web tools as separate rows. Its availability comes from Aurora's
actual Turn registration dependencies; tools without their backing integration
and the whole section for an empty set are omitted. Memory/Scratch maintenance
and `list_commands` are internal and never listed. These rows are HTML-only:
`/commands.json`, autocomplete and `!help` remain chat-trigger-only.

In chat, `!help <befehl>` (also `!commands`, `!hilfe`) answers with one
entry from that catalog. The `!` is optional, case is ignored, and aliases
resolve, so `!help grok` and `!help @grok` both find `!ai`. A hidden command
gets the same "kenn ich nicht" reply as an unknown one. A bare `!help`
links to `/commands` when the dashboard is enabled, otherwise it explains
`!help <befehl>`. Per-user cooldown: `cooldowns.help` (`/settings →
Cooldowns`).
