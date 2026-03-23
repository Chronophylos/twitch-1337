# Fastest 1337 Message Tracking

## Summary

Extend the 1337 handler to track the fastest message each day (sub-1-second after 13:37:00.000 Berlin time), mention the fastest user with their millisecond time in the stats message, announce when it's a new all-time record, and persist a per-user all-time leaderboard.

## Data Changes

### Daily tracking

Replace `Arc<Mutex<HashSet<String>>>` with `Arc<Mutex<HashMap<String, Option<u64>>>>`.

- Key: username
- Value: `Some(ms)` for messages in the first second (0-999ms after 13:37:00.000), `None` for later messages
- First message per user wins (no overwrite on duplicate)
- Same `MAX_USERS` cap (10,000)

### Persistent leaderboard (`leaderboard.ron`)

```rust
struct PersonalBest {
    ms: u64,
    date: NaiveDate,
}
// File contains: HashMap<String, PersonalBest>
```

- Loaded at 13:36 when the handler wakes
- Updated and saved at 13:38 after processing today's results
- Each user's entry is only updated if today's time is better than their stored best
- File created fresh if missing; logged warning and fresh start if corrupted

## Monitor Changes

In `monitor_1337_messages()`, after validating the message:

1. Compute `ms = server_timestamp.second() * 1000 + server_timestamp.timestamp_subsec_millis()`
2. If `ms < 1000`: insert `(username, Some(ms))`
3. Otherwise: insert `(username, None)`
4. Only insert if username not already present (first message wins)

## Stats Message Changes

At 13:38, before sending the message:

1. Lock the HashMap, get user count and list
2. Generate the existing count-based message via `generate_stats_message()`
3. Find the day's fastest: filter for `Some(ms)`, pick the minimum value
4. Load leaderboard from `leaderboard.ron`
5. Update leaderboard: for each sub-1s user today, update their personal best if this time is better
6. Determine if the day's fastest is a new all-time record (beats the best `ms` across all leaderboard entries)
7. If there is a fastest user, append to the message: `" | {user} war mass schnellste mit {ms}ms"`
8. If it is a new all-time record, further append: `" - neuer Rekord!"`
9. Save updated leaderboard to `leaderboard.ron`
10. Send the message

## Edge Cases

- **No sub-1s messages:** No fastest mention appended. Normal stats message only.
- **Leaderboard file missing:** Created as empty HashMap on first load.
- **Leaderboard file corrupted:** Log warning, start with empty HashMap, don't crash.
- **Millisecond tie:** First user inserted into the HashMap wins for the day.
- **`expected_latency` config:** Not factored in. Raw server timestamps used for consistency.
- **All-time record on first day:** Any sub-1s time is automatically a record when leaderboard is empty.

## Files Changed

- `src/main.rs`:
  - `monitor_1337_messages()` — HashMap instead of HashSet, compute and store ms
  - `run_1337_handler()` — load leaderboard at 13:36, update and save at 13:38
  - `generate_stats_message()` — accept fastest info, append to message
  - New: `PersonalBest` struct, leaderboard load/save functions
- New file: `leaderboard.ron` (created at runtime, gitignored)

## Out of Scope

- Displaying the full leaderboard in chat
- Web dashboard for leaderboard
- Per-user personal best announcements
