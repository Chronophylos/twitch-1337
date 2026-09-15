pub const PRESET: &str = include_str!("../assets/prompts/preset.ron");
pub const DREAMER: &str = include_str!("../assets/prompts/dreamer.md");
pub const RECAP: &str = include_str!("../assets/prompts/recap.md");
pub const SEED_SOUL: &str = include_str!("../assets/prompts/seed_soul.md");

/// Bodies referenced by `File(...)` entries in [`PRESET`], keyed by the path
/// the preset names. Baked, not read from disk — a new `File(...)` entry needs
/// a line here or the preset fails to resolve.
pub const PRESET_FILES: &[(&str, &str)] = &[
    (
        "aurora/core.md",
        include_str!("../assets/prompts/aurora/core.md"),
    ),
    (
        "aurora/instructions.md",
        include_str!("../assets/prompts/aurora/instructions.md"),
    ),
    (
        "grok/core.md",
        include_str!("../assets/prompts/grok/core.md"),
    ),
    (
        "grok/instructions.md",
        include_str!("../assets/prompts/grok/instructions.md"),
    ),
];
