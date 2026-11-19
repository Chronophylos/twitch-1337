# Quake Magnitude Stored as a Float

The quake alert threshold stays stored as integer tenths
(`alerts.quake.min_mag_tenths: u32`, `25` = M2.5). It is not migrated to a
natural-unit `f64` (`min_mag: 2.5`).

## Why this is out of scope

The user-facing part of the request is already solved. The `/settings → Alerts`
card shows and accepts a decimal magnitude (`2.5`) and converts it to tenths on
save. Viewers and mods never see the scaled integer. Changing the storage unit
would alter only the RON on disk and one division in `quake::url`, the field's
sole consumer.

The cost is not zero:

- **A settings migration.** Existing `settings.ron` files carry
  `min_mag_tenths: Some(25)`. Renaming the field means reading the legacy key
  and dividing by ten, or adding another migration sentinel. That is permanent
  code for no visible gain.
- **A float knob range.** The `#[knob(max = 100)]` bound on tenths becomes a
  float range, and the template's decimal-display shim has to be reworked.

### The premise in the original request was wrong

The request, and the doc comment on `QuakeAlerts`, assumed an `f64` field
would force `Eq` off the whole `Settings` tree and break the snapshot diffing
behind the audit log. It would not. `AiPrefill.threshold` is already an `f64`.
It keeps `Eq` through a manual `PartialEq` that compares `to_bits()` plus
`impl Eq`, and `FormValue for f64` already exists for the form layer. If this
field ever needs to change for another reason, follow that pattern. Dropping
`Eq` from the tree is not required.

## Prior requests

- #495 — "explore(alerts): quake min_mag_tenths schema — accept more than u32"
