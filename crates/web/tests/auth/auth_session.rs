use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use twitch_1337_web::auth::Role;
use twitch_1337_web::auth::session::{SessionTable, TwitchRefreshToken};
use twitch_1337_web::clock::Clock;

struct StubClock(std::sync::Mutex<Timestamp>);

impl Clock for StubClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap()
    }
}

impl StubClock {
    fn new(t: Timestamp) -> Self {
        Self(std::sync::Mutex::new(t))
    }
}

#[tokio::test]
async fn sessions_survive_reload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sessions.ron");
    let clock = Arc::new(StubClock::new(
        "2026-01-01T00:00:00Z".parse::<Timestamp>().unwrap(),
    ));

    let table = SessionTable::load(clock.clone(), path.clone());
    let (id, _csrf) = table
        .insert(twitch_1337_web::auth::session::NewSession {
            user_id: "12345".into(),
            user_login: "alice".into(),
            role: Role::Mod,
            avatar_url: Some("https://example.invalid/a.png".into()),
            is_broadcaster: false,
            refresh_token: Some(TwitchRefreshToken::new("rt-alice".into())),
        })
        .expect("insert");
    table.persist().await;
    drop(table);

    // Simulate a bot restart: a fresh table reading the same file must still
    // recognise the sid the user's signed cookie carries.
    let reloaded = SessionTable::load(clock.clone(), path);
    let got = reloaded
        .get_and_touch(&id, Duration::from_secs(7200))
        .expect("session survives reload");
    assert_eq!(got.user_login, "alice");
    assert_eq!(got.role, Role::Mod);
    // The Mod recheck spends this token; losing it on restart would log every
    // moderator out at their next recheck.
    assert_eq!(
        got.refresh_token,
        Some(TwitchRefreshToken::new("rt-alice".into()))
    );
    // last_role_check is reset on load so the gate re-verifies the role on
    // the first post-restart request (a mod demoted while the bot was down
    // must not keep access on stale state).
    assert_eq!(
        got.last_role_check,
        Timestamp::MIN,
        "restored session must force a fresh role check"
    );
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test fixtures seed files directly"
)]
fn corrupt_session_file_starts_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sessions.ron");
    std::fs::write(&path, "this is not valid ron {{{").expect("write garbage");
    let clock = Arc::new(StubClock::new(
        "2026-01-01T00:00:00Z".parse::<Timestamp>().unwrap(),
    ));
    // Must not panic — a corrupt file costs one re-login, never a crash.
    let table = SessionTable::load(clock, path);
    assert!(
        table
            .get_and_touch("deadbeef", Duration::from_secs(7200))
            .is_none()
    );
}
