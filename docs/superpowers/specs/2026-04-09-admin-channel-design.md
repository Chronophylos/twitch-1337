# Admin Channel Design

## Overview

An optional second Twitch channel the bot joins for testing commands in isolation. Only the broadcaster can interact with the bot there. No 1337 tracker, no scheduled messages, no latency monitor — just the command dispatcher.

## Motivation

Provide a private sandbox channel where the broadcaster can test new bot features and commands without affecting the main stream chat.

## Config Change

Add `admin_channel: Option<String>` to `TwitchConfiguration`:

```toml
[twitch]
channel = "main_channel"
admin_channel = "my_test_channel"
```

Omitting the field means no admin channel (current behavior preserved).

## Connection

In `setup_and_verify_twitch_client()`, after joining the main channel, also join the admin channel if configured. Both channels share the same IRC connection and broadcast channel — no second client needed.

## Command Dispatcher Changes

The command dispatcher already receives all messages from the broadcast channel. The change is a gate at dispatch time:

1. Check if the message is from the admin channel.
2. If yes: only dispatch if the sender has the `broadcaster` badge. Silently ignore everyone else.
3. If no: current behavior unchanged.

The dispatcher receives the admin channel name as `Option<String>` alongside existing parameters.

## What Stays Main-Channel-Only

The 1337 handler, scheduled message handler, and latency monitor only care about the main channel by nature of their logic. No changes needed — they already filter by their own criteria and will ignore messages from the admin channel.

## Scope of Changes

| File | Change |
|------|--------|
| `TwitchConfiguration` (main.rs) | Add `admin_channel: Option<String>` field |
| `config.toml.example` | Add commented-out `admin_channel` example |
| `setup_and_verify_twitch_client()` | Join second channel if configured |
| `run_generic_command_handler()` | Accept admin channel name, add broadcaster-only gate |
| `CLAUDE.md` | Document the new config field |

## Authorization

In the admin channel, **only the broadcaster badge** grants access. No moderator access, no `hidden_admins` override. This is intentional — the admin channel is the broadcaster's private testing space.

## Non-Goals

- Running non-command handlers (1337, schedules, latency) in the admin channel
- Per-command enable/disable for the admin channel
- Separate admin channel credentials or connection
