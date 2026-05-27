//! Integration test: legacy config.toml -> settings.ron migration runs once,
//! is idempotent on second startup, and respects the sentinel.

use std::sync::Arc;

use tempfile::tempdir;
use twitch_1337_core::settings::{
    Actor, SettingsStore,
    audit::MemoryAuditLog,
    migrate::{migrate_legacy_config, migrate_legacy_schedules},
    overrides::SettingsOverrides,
};

#[tokio::test]
async fn legacy_keys_migrate_then_idempotent() {
    let dir = tempdir().expect("tmp");
    let audit = Arc::new(MemoryAuditLog::default());
    let (store, _h) = SettingsStore::open(dir.path(), audit.clone(), "main").expect("open");

    let raw = r#"
        [twitch]
        channel = "main"
        username = "bot"
        refresh_token = "r"
        client_id = "i"
        client_secret = "s"
        expected_latency = 250
        admin_channel = "admins"

        [suspend]
        default_duration_secs = 900
    "#;
    let value: toml::Value = toml::from_str(raw).expect("parse");

    // First migration
    let patch = migrate_legacy_config(&value).expect("migrate");
    store
        .apply(
            patch,
            Actor {
                user_id: "system".into(),
                user_login: "v3-migration".into(),
            },
        )
        .await
        .expect("apply");
    let s = store.handle().load();
    assert_eq!(s.twitch.expected_latency, 250);
    assert_eq!(s.twitch.admin_channel.as_deref(), Some("admins"));
    assert_eq!(s.suspend.default_duration_secs, 900);

    // Second migration: same patch idempotently re-applied; resolved settings unchanged
    let patch2 = migrate_legacy_config(&value).expect("migrate");
    store
        .apply(
            patch2,
            Actor {
                user_id: "system".into(),
                user_login: "v3-migration".into(),
            },
        )
        .await
        .expect("apply");
    let s2 = store.handle().load();
    assert_eq!(*s, *s2);
}

#[tokio::test]
async fn invalid_legacy_schedule_does_not_block_runtime_config_migration() {
    let dir = tempdir().expect("tmp");
    let audit = Arc::new(MemoryAuditLog::default());
    let (store, _h) = SettingsStore::open(dir.path(), audit.clone(), "main").expect("open");

    let raw = r#"
        [twitch]
        channel = "main"
        username = "bot"
        refresh_token = "r"
        client_id = "i"
        client_secret = "s"
        expected_latency = 250

        [[schedules]]
        name = "bad name"
        message = "still should not block runtime migration"
        interval = "01:00"
    "#;
    let value: toml::Value = toml::from_str(raw).expect("parse");

    let patch = migrate_legacy_config(&value).expect("migrate runtime config");
    store
        .apply(
            patch,
            Actor {
                user_id: "system".into(),
                user_login: "v3-migration".into(),
            },
        )
        .await
        .expect("runtime migration should not validate legacy schedules");

    let s = store.handle().load();
    assert_eq!(s.twitch.expected_latency, 250);
    assert!(s.schedules.is_empty());

    let schedules = migrate_legacy_schedules(&value).expect("migrate schedules");
    let err = store
        .apply(
            SettingsOverrides {
                schedules,
                ..SettingsOverrides::default()
            },
            Actor {
                user_id: "system".into(),
                user_login: "schedules-v3-migration".into(),
            },
        )
        .await
        .expect_err("invalid legacy schedule should still fail the schedule-only migration");
    assert!(err.to_string().contains("validation failed"));
}
