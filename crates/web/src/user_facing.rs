//! User-facing error messages for form re-renders.
//!
//! Handlers that preserve the user's draft on validation failure thread
//! these strings into the template `error` field instead of duplicating
//! per-variant match arms at every call site.

use twitch_1337_core::ai::memory::store::WriteError;
use twitch_1337_core::ping::PingError;

use crate::error::WebError;

/// Maps a domain error to the short message shown in form error banners.
pub trait UserFacing {
    fn user_message(&self) -> String;
}

impl UserFacing for WriteError {
    fn user_message(&self) -> String {
        match self {
            WriteError::Full { limit, actual } => {
                format!("exceeds byte cap ({actual} bytes, cap {limit})")
            }
            WriteError::ScratchFull => "scratch collection full".to_owned(),
            WriteError::InvalidSlug => "invalid slug".to_owned(),
            WriteError::ReservedSlug => "reserved slug".to_owned(),
            WriteError::NotOwner => "scratch file belongs to someone else".to_owned(),
            // `Io` should bubble via [`write_error_for_form`], not display.
            WriteError::Io(e) => format!("{e:#}"),
        }
    }
}

impl UserFacing for PingError {
    fn user_message(&self) -> String {
        match self {
            PingError::NotFound { name } => format!("ping `{name}` does not exist"),
            PingError::AlreadyExists { name } => format!("ping `{name}` already exists"),
            PingError::ReservedCommand { name } => format!("`{name}` is a built-in command"),
            PingError::EmptyName => "name must not be empty".to_owned(),
            PingError::InvalidName => "name may only contain letters, digits, - and _".to_owned(),
            PingError::ControlChars => {
                "template must not contain control characters (e.g. line breaks)".to_owned()
            }
            PingError::InvalidMember { .. } => {
                "username must be 1\u{2013}25 chars: lowercase letters, digits, underscore"
                    .to_owned()
            }
            PingError::AlreadyMember { user, .. } => format!("`{user}` is already a member"),
            PingError::NotMember { user, .. } => format!("`{user}` is not a member"),
            // Internal failures bubble via [`ping_error_for_form`], not display.
            PingError::Unavailable | PingError::Persist(_) => "internal error".to_owned(),
        }
    }
}

/// `WriteError::Io` bubbles to `WebError::Internal`; other variants become
/// an inline form message.
pub fn write_error_for_form(err: WriteError) -> Result<String, WebError> {
    match err {
        WriteError::Io(e) => Err(WebError::Internal(e)),
        e => Ok(e.user_message()),
    }
}

/// `PingError::{Persist, Unavailable}` bubble to `WebError::Internal`;
/// other variants become an inline form message.
pub fn ping_error_for_form(err: PingError) -> Result<String, WebError> {
    match err {
        PingError::Unavailable | PingError::Persist(_) => {
            Err(WebError::Internal(eyre::Report::new(err)))
        }
        e => Ok(e.user_message()),
    }
}
