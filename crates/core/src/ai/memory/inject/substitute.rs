//! Prompt-file substitution: baked prompt consts plus `{speaker_*}`-style
//! template vars.

use rand::Rng as _;

// Prompt templates are code (#321): baked into the binary and used directly at
// runtime by the !ai handler and the dreamer ritual. The repo copy
// (`crates/data/assets/prompts/`) is the single source of truth — no on-disk
// override, no drift. Substitution of `{speaker_*}`/`{date}`/`{model}`/`{channel}`
// / `{reply_target}`/`{reply_hard}` happens in `substitute` below.
pub(crate) const PROMPT_SYSTEM: &str = twitch_1337_data::prompts::SYSTEM;
pub(crate) const PROMPT_GROK: &str = twitch_1337_data::prompts::GROK;
pub(crate) const PROMPT_INSTRUCTIONS: &str = twitch_1337_data::prompts::AI_INSTRUCTIONS;
pub(crate) const PROMPT_DREAMER: &str = twitch_1337_data::prompts::DREAMER;

/// Generate a 16-hex-char nonce for one prompt build.
pub fn fresh_nonce() -> String {
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Copy)]
pub struct SubstitutionVars<'a> {
    pub speaker_username: &'a str,
    pub speaker_display: &'a str,
    pub speaker_user_id: &'a str,
    pub speaker_role: &'a str,
    pub channel: &'a str,
    pub date: &'a str,
    pub model: &'a str,
    pub model_id: &'a str,
    pub reply_target: &'a str,
    pub reply_hard: &'a str,
}

pub fn substitute(template: &str, v: SubstitutionVars<'_>) -> String {
    template
        .replace("{speaker_username}", v.speaker_username)
        .replace("{speaker_display}", v.speaker_display)
        .replace("{speaker_user_id}", v.speaker_user_id)
        .replace("{speaker_role}", v.speaker_role)
        .replace("{channel}", v.channel)
        .replace("{date}", v.date)
        .replace("{model}", v.model)
        .replace("{model_id}", v.model_id)
        .replace("{reply_target}", v.reply_target)
        .replace("{reply_hard}", v.reply_hard)
}
