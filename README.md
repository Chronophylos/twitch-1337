<p align="center">
  <img src="docs/readme/hero.svg" alt="The Ɛ:3 mark splitting open over a glowing planet horizon, with twitch-1337 between its wings" width="100%">
</p>

<p align="center"><em>A Twitch bot for one channel, with a mind of its own.</em></p>

<p align="center">
  <a href="https://github.com/Chronophylos/twitch-1337/actions/workflows/ci.yml"><img src="https://github.com/Chronophylos/twitch-1337/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/Chronophylos/twitch-1337/actions/workflows/docker.yml"><img src="https://github.com/Chronophylos/twitch-1337/actions/workflows/docker.yml/badge.svg" alt="Docker"></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-c79bff.svg" alt="License: MIT OR Apache-2.0"></a>
  <a href="https://doc.rust-lang.org/edition-guide/rust-2024/"><img src="https://img.shields.io/badge/rust-2024-c79bff.svg" alt="Rust Edition 2024"></a>
</p>

Every day at **13:37** Berlin time, chat races to type `1337`. **Ɛ:3** keeps
score. Over the years it picked up a few other hobbies.

## The 13:37 race

<img src="docs/readme/race.svg" alt="A chat log at 13:37 with millisecond timestamps, the bot's verdict, and a podium of the three fastest personal bests" width="100%">

> For one minute a day, Ɛ:3 watches chat with a stopwatch. A latency probe
> pings the IRC server every five minutes, so the bot wakes up early enough to
> catch the very first message, to the millisecond. Sub-second personal bests
> go on the board. The slowest go in the hall of shame.

## Aurora dreams

<img src="docs/readme/dreams.svg" alt="A character sheet before and after the nightly dreamer: stale lines struck out, new relationships and interests added" width="100%">

> Aurora, the AI who lives in chat, keeps a character sheet on every regular,
> a lore file for the channel and a soul file for herself. Every night the
> dreamer reads the day's transcript and rewrites them: what will still matter
> in two weeks moves up, the rest fades. Each sheet is capped at 4 KiB, so
> forgetting is part of the design.

## Flights

<img src="docs/readme/flights.svg" alt="A route from FRA to PMI with the aircraft two thirds along, next to the chat messages the tracker posted from takeoff to landing, and a graded Volanta landing" width="100%">

> `!track` follows a real aircraft over ADS-B from takeoff to touchdown and
> polls faster while it climbs, descends and lands. Regulars who fly in
> Volanta get their landings graded the moment the wheels settle: sink rate and
> G-force, one grade worse for every bounce.

## Under the hood

```mermaid
flowchart LR
    irc(["Twitch IRC"]) --> bus{{"broadcast channel"}}
    bus --> race["13:37 race"]
    bus --> commands["commands"]
    bus --> aurora["Aurora"]
    bus --> flights["flights"]
    bus --> more["…"]
    aurora -. "own Twitch account" .-> helix(["Helix chat"])
    dash["dashboard"] -- "edits" --> settings[("settings.ron")]
    settings -. "live" .-> commands
    settings -. "live" .-> aurora
```

<img src="docs/readme/flex.svg" alt="static musl binary, ~29 MB · Docker image FROM scratch · TLS on ring, no OpenSSL · panics denied by lint · every AI turn traced · settings apply live" width="100%">

> One IRC connection, one broadcast channel, one task per feature. A slow
> feature lags on its own; a crashed one takes the whole bot down so it comes
> back clean, instead of limping on half-alive.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.

<p align="center"><img src="docs/brand/mark.svg" alt="Ɛ:3" width="40"></p>
