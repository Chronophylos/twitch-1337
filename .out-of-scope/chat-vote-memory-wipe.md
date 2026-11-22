# Chat Vote to Wipe AI Memory

Chat cannot vote to wipe the AI's memory (SOUL, LORE, Character Sheets, Scratch).
Memory is changed by the agents' own tools, the nightly dreamer, and the owner on
the dashboard. A chat majority is not one of those paths.

## Why this is out of scope

**A wipe is destructive and has no undo.** Memory is the accumulated result of
months of dreamer runs: the bot's SOUL, the channel's LORE, and one Character
Sheet per regular. A vote would delete all of it based on whoever happens to be
in chat at that moment. Votes are easy to brigade (alt accounts, a raid, a joke
that snowballs), and nothing can restore the files afterwards.

**It crosses deliberate write boundaries.** Per ADR-0003 and the memory design,
SOUL is dreamer-only: no chat Turn, from either agent, can write it. Character
Sheet ids cannot be minted by the model. A chat-triggered wipe of SOUL/LORE/users
would be a far broader write path than anything the agents themselves are
allowed, and it would be triggered by viewers rather than by the bot or its
owner.

**The real need is already served, more precisely.** When the bot has learned
something wrong or unwanted, the fix is targeted, not total. The dashboard
already does this for mods and the owner:

- `/memory/soul` and `/memory/lore`: view and edit
- `/memory/users/{id}`: edit, and `/memory/users/{id}/forget` to delete one
  viewer's Character Sheet
- `/memory/scratch/{slug}/delete`: delete a Scratch note

A viewer who wants the bot to forget them can ask a mod to use "forget". Chat
moderation of the bot's knowledge goes through people who can see what is
actually being deleted.

## Prior requests

- #633 — "Chat vote to wipe the AI's memory"
