//! Reading `settings.ron` from disk: parse and corruption quarantine. A file
//! that fails to parse is renamed aside so the bot can still boot on compile
//! defaults.

use std::path::Path;

use tracing::warn;

use crate::settings::SettingsOverrides;
use crate::settings::{SCHEMA_VERSION, SettingsError};

pub(super) fn load_or_quarantine(path: &Path) -> Result<SettingsOverrides, SettingsError> {
    match load_overrides(path) {
        Ok(Some(o)) => Ok(o),
        Ok(None) => Ok(SettingsOverrides::default()),
        Err(e) if is_future_version(path) => {
            // A newer build wrote this file; keep it for the roll-forward.
            crate::log_handled!(
                warn,
                e,
                "settings.ron from a newer build does not parse; using defaults, leaving the file alone"
            );
            Ok(SettingsOverrides::default())
        }
        Err(e) => {
            crate::log_handled!(warn, e, "settings.ron is corrupt; quarantining");
            quarantine(path)?;
            Ok(SettingsOverrides::default())
        }
    }
}

fn load_overrides(path: &Path) -> Result<Option<SettingsOverrides>, SettingsError> {
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(path)?;
    let parsed: SettingsOverrides = ron::from_str(&body)?;
    Ok(Some(parsed))
}

pub(super) async fn load_overrides_async(
    path: &Path,
) -> Result<Option<SettingsOverrides>, SettingsError> {
    if !tokio::fs::try_exists(path).await? {
        return Ok(None);
    }
    let body = tokio::fs::read_to_string(path).await?;
    let parsed: SettingsOverrides = ron::from_str(&body)?;
    Ok(Some(parsed))
}

/// True when `settings.ron` carries a `schema_version` newer than this build.
/// Peeks only that field, so it works even when the rest of the file uses
/// shapes this build can't parse.
pub(super) fn is_future_version(path: &Path) -> bool {
    #[derive(serde::Deserialize)]
    struct Peek {
        #[serde(default)]
        schema_version: u32,
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|body| ron::from_str::<Peek>(&body).ok())
        .is_some_and(|p| p.schema_version > SCHEMA_VERSION)
}

pub(super) fn quarantine(path: &Path) -> Result<(), SettingsError> {
    if !path.exists() {
        return Ok(());
    }
    let ts = jiff::Timestamp::now().as_second();
    let target = path.with_extension(format!("ron.quarantine-{ts}"));
    std::fs::rename(path, &target)?;
    warn!(target = ?target, "settings.ron quarantined");
    Ok(())
}
