# Security policy

## Reporting a vulnerability

Report vulnerabilities privately through GitHub:
[Security → Report a vulnerability](https://github.com/Chronophylos/twitch-1337/security/advisories/new).
Do not open a public issue or pull request for a security problem.

Include what an attacker needs (chat viewer, moderator, dashboard login, network
position), the steps to reproduce, and the impact. You will get a reply within a
week.

## Supported versions

There are no releases. Only the current `main` branch is supported; the running
bot is redeployed from it on every merge.

## Scope

In scope: the bot's chat commands and AI tools, the web dashboard and its auth,
and the build and CI configuration in this repository.

Out of scope: Twitch, the LLM providers, and the other third-party APIs the bot
calls, plus findings that need access to the host or to `config.toml`.
