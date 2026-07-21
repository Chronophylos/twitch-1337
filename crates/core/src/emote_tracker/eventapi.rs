//! 7TV EventAPI v3 websocket client and `emote_set.update` dispatch decoder.
//!
//! Nullable `actor` on dispatches is expected — see [SevenTV#256](https://github.com/SevenTV/SevenTV/issues/256).

use eyre::{Result, WrapErr as _, eyre};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use super::reconcile::{ActorRef, DesiredEmote};
use super::types::{CatalogMutation, NewSetEvent, SetChangeBatch, SetEventKind};

pub const EVENTAPI_URL: &str = "wss://events.7tv.io/v3";

const OP_DISPATCH: u64 = 0;
const OP_HELLO: u64 = 1;
const OP_HEARTBEAT: u64 = 2;
const OP_ACK: u64 = 5;
const OP_ERROR: u64 = 6;
const OP_END_OF_STREAM: u64 = 7;
const OP_SUBSCRIBE: u64 = 35;
const OP_UNSUBSCRIBE: u64 = 36;

/// EventAPI close codes where the server asks clients to reconnect.
///
/// From the [EventAPI close-code table](https://github.com/SevenTV/EventAPI)
/// (`Reconnect? = Yes`) plus `4012` which production sends hourly as
/// `End of Stream` / `message=Reconnect` (scheduled session rotation; not in the
/// published table yet).
pub fn is_expected_reconnect(code: u16) -> bool {
    matches!(code, 4000 | 4006 | 4007 | 4008 | 4012)
}

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type EventApiSink = futures_util::stream::SplitSink<WsStream, Message>;
pub type EventApiStream = futures_util::stream::SplitStream<WsStream>;

pub struct EventApiConnection {
    pub sink: EventApiSink,
    pub stream: EventApiStream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchEmoteSetUpdate {
    pub set_id: String,
    pub actor: Option<ActorRef>,
    pub pushed: Vec<DesiredEmote>,
    pub pulled: Vec<DesiredEmote>,
    pub updated: Vec<(DesiredEmote, DesiredEmote)>,
}

/// Active emote-set change on a Twitch connection (`user.update`).
///
/// Chatterino parses `body.updated[]` entries with `key == "connections"` and a
/// nested `emote_set` field change — see `UserConnectionUpdateDispatch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchUserActiveSetChanged {
    pub user_id: String,
    pub old_set_id: String,
    pub new_set_id: String,
    pub actor: Option<ActorRef>,
    pub connection_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedEvent {
    Hello {
        heartbeat_interval_ms: Option<u64>,
    },
    Heartbeat,
    Dispatch(DispatchEmoteSetUpdate),
    UserActiveSetChanged(DispatchUserActiveSetChanged),
    Ack,
    /// Server error frame (`op` 6). Connection may continue or close next.
    Error {
        code: Option<u16>,
        message: String,
    },
    /// Server will close imminently (`op` 7). Includes EventAPI close code.
    EndOfStream {
        code: u16,
        message: String,
    },
    Other {
        op: u64,
    },
}

#[derive(Debug, Deserialize)]
struct RootMessage {
    op: u64,
    #[serde(default)]
    d: Value,
}

/// Parse a single EventAPI text frame. Returns `Ok(None)` for blank input.
pub fn parse_server_message(text: &str) -> Result<Option<ParsedEvent>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let root: RootMessage =
        serde_json::from_str(trimmed).wrap_err("failed to parse EventAPI JSON")?;

    match root.op {
        OP_DISPATCH => parse_dispatch(root.d),
        OP_HELLO => {
            let heartbeat_interval_ms = root.d.get("heartbeat_interval").and_then(Value::as_u64);
            Ok(Some(ParsedEvent::Hello {
                heartbeat_interval_ms,
            }))
        }
        OP_HEARTBEAT => Ok(Some(ParsedEvent::Heartbeat)),
        OP_ACK => Ok(Some(ParsedEvent::Ack)),
        OP_ERROR => {
            let code = root
                .d
                .get("code")
                .and_then(Value::as_u64)
                .and_then(|c| u16::try_from(c).ok());
            let message = root
                .d
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            Ok(Some(ParsedEvent::Error { code, message }))
        }
        OP_END_OF_STREAM => {
            let code = root
                .d
                .get("code")
                .and_then(Value::as_u64)
                .and_then(|c| u16::try_from(c).ok())
                .unwrap_or(0);
            let message = root
                .d
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(Some(ParsedEvent::EndOfStream { code, message }))
        }
        op => Ok(Some(ParsedEvent::Other { op })),
    }
}

/// JSON text frame for `op` 35 subscribe to `emote_set.update` for one set.
///
/// Official EventAPI / Chatterino use `object_id` for set membership changes.
/// Channel `ctx` is for cosmetics/entitlements and does **not** deliver personal
/// set add/remove/rename events.
pub fn subscribe_emote_set_update_message(emote_set_id: &str) -> String {
    serde_json::json!({
        "op": OP_SUBSCRIBE,
        "d": {
            "type": "emote_set.update",
            "condition": {
                "object_id": emote_set_id,
            }
        }
    })
    .to_string()
}

/// JSON text frame for `op` 36 unsubscribe from `emote_set.update` on one set.
pub fn unsubscribe_emote_set_update_message(emote_set_id: &str) -> String {
    serde_json::json!({
        "op": OP_UNSUBSCRIBE,
        "d": {
            "type": "emote_set.update",
            "condition": {
                "object_id": emote_set_id,
            }
        }
    })
    .to_string()
}

/// JSON text frame for `op` 35 subscribe to `user.update` for a 7TV user.
///
/// Used to detect active emote-set switches on the Twitch connection (Chatterino
/// `subscribeUser`).
pub fn subscribe_user_update_message(seventv_user_id: &str) -> String {
    serde_json::json!({
        "op": OP_SUBSCRIBE,
        "d": {
            "type": "user.update",
            "condition": {
                "object_id": seventv_user_id,
            }
        }
    })
    .to_string()
}

fn parse_dispatch(data: Value) -> Result<Option<ParsedEvent>> {
    let event_type = data
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("dispatch missing d.type"))?;

    match event_type {
        "emote_set.update" => parse_emote_set_update(data),
        "user.update" => parse_user_update(data),
        _ => Ok(None),
    }
}

fn parse_emote_set_update(data: Value) -> Result<Option<ParsedEvent>> {
    let body = data
        .get("body")
        .ok_or_else(|| eyre!("emote_set.update dispatch missing d.body"))?;

    let set_id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("emote_set.update body missing id"))?
        .to_string();

    let actor = body
        .get("actor")
        .filter(|value| !value.is_null())
        .map(parse_actor);

    let pushed = parse_emote_list(body.get("pushed"), "value")?;
    let pulled = parse_emote_list(body.get("pulled"), "old_value")?;
    let updated = parse_updated_list(body.get("updated"))?;

    Ok(Some(ParsedEvent::Dispatch(DispatchEmoteSetUpdate {
        set_id,
        actor,
        pushed,
        pulled,
        updated,
    })))
}

fn parse_user_update(data: Value) -> Result<Option<ParsedEvent>> {
    let body = data
        .get("body")
        .ok_or_else(|| eyre!("user.update dispatch missing d.body"))?;

    let user_id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("user.update body missing id"))?
        .to_string();

    let actor = body
        .get("actor")
        .filter(|value| !value.is_null())
        .map(parse_actor);

    let Some(updated) = body.get("updated").and_then(Value::as_array) else {
        return Ok(None);
    };

    for entry in updated {
        if entry.get("key").and_then(Value::as_str) != Some("connections") {
            continue;
        }
        let connection_index = entry
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|i| usize::try_from(i).ok())
            .unwrap_or(0);
        let Some(fields) = entry.get("value").and_then(Value::as_array) else {
            continue;
        };
        for field in fields {
            if field.get("key").and_then(Value::as_str) != Some("emote_set") {
                continue;
            }
            let old_set_id = field
                .get("old_value")
                .and_then(|v| v.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| eyre!("user.update emote_set missing old_value.id"))?
                .to_string();
            let new_set_id = field
                .get("value")
                .and_then(|v| v.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| eyre!("user.update emote_set missing value.id"))?
                .to_string();
            if old_set_id == new_set_id {
                continue;
            }
            return Ok(Some(ParsedEvent::UserActiveSetChanged(
                DispatchUserActiveSetChanged {
                    user_id,
                    old_set_id,
                    new_set_id,
                    actor,
                    connection_index,
                },
            )));
        }
    }

    Ok(None)
}

fn parse_actor(value: &Value) -> ActorRef {
    ActorRef {
        id: value.get("id").and_then(Value::as_str).map(str::to_string),
        username: value
            .get("username")
            .and_then(Value::as_str)
            .map(str::to_string),
        display_name: value
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn parse_emote_list(field: Option<&Value>, value_key: &str) -> Result<Vec<DesiredEmote>> {
    let Some(items) = field else {
        return Ok(Vec::new());
    };
    let Some(items) = items.as_array() else {
        return Ok(Vec::new());
    };

    items
        .iter()
        .map(|item| {
            let value = item
                .get(value_key)
                .ok_or_else(|| eyre!("change field missing {value_key}"))?;
            parse_emote_value(value)
        })
        .collect()
}

fn parse_updated_list(field: Option<&Value>) -> Result<Vec<(DesiredEmote, DesiredEmote)>> {
    let Some(items) = field else {
        return Ok(Vec::new());
    };
    let Some(items) = items.as_array() else {
        return Ok(Vec::new());
    };

    items
        .iter()
        .map(|item| {
            let old_value = item
                .get("old_value")
                .ok_or_else(|| eyre!("updated change missing old_value"))?;
            let new_value = item
                .get("value")
                .ok_or_else(|| eyre!("updated change missing value"))?;
            Ok((parse_emote_value(old_value)?, parse_emote_value(new_value)?))
        })
        .collect()
}

fn parse_emote_value(value: &Value) -> Result<DesiredEmote> {
    let emote_id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("emote value missing id"))?
        .to_string();
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("emote value missing name"))?
        .to_string();
    let actor_id = value
        .get("actor_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(DesiredEmote {
        emote_id,
        name,
        actor_id,
    })
}

/// Map a live `emote_set.update` dispatch into catalog mutations + audit events.
pub fn live_change_to_batch(at: i64, dispatch: &DispatchEmoteSetUpdate) -> SetChangeBatch {
    let actor = dispatch.actor.as_ref();
    let mut mutations = Vec::new();
    let mut events = Vec::new();

    for emote in &dispatch.pushed {
        mutations.push(CatalogMutation::Upsert {
            emote_id: emote.emote_id.clone(),
            name: emote.name.clone(),
            active: true,
        });
        let mut event = NewSetEvent {
            at,
            kind: SetEventKind::Add,
            emote_id: Some(emote.emote_id.clone()),
            old_name: None,
            new_name: Some(emote.name.clone()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
            requester_login: None,
            requester_twitch_id: None,
            via: None,
            attribution_method: None,
        };
        apply_actor(&mut event, actor);
        events.push(event);
    }

    for emote in &dispatch.pulled {
        mutations.push(CatalogMutation::Deactivate {
            emote_id: emote.emote_id.clone(),
        });
        let mut event = NewSetEvent {
            at,
            kind: SetEventKind::Remove,
            emote_id: Some(emote.emote_id.clone()),
            old_name: Some(emote.name.clone()),
            new_name: None,
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
            requester_login: None,
            requester_twitch_id: None,
            via: None,
            attribution_method: None,
        };
        apply_actor(&mut event, actor);
        events.push(event);
    }

    for (old_emote, new_emote) in &dispatch.updated {
        if old_emote.emote_id == new_emote.emote_id {
            mutations.push(CatalogMutation::Upsert {
                emote_id: new_emote.emote_id.clone(),
                name: new_emote.name.clone(),
                active: true,
            });
            let mut event = NewSetEvent {
                at,
                kind: SetEventKind::Rename,
                emote_id: Some(new_emote.emote_id.clone()),
                old_name: Some(old_emote.name.clone()),
                new_name: Some(new_emote.name.clone()),
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: None,
                gap_to: None,
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            };
            apply_actor(&mut event, actor);
            events.push(event);
        } else {
            mutations.push(CatalogMutation::Deactivate {
                emote_id: old_emote.emote_id.clone(),
            });
            let mut remove = NewSetEvent {
                at,
                kind: SetEventKind::Remove,
                emote_id: Some(old_emote.emote_id.clone()),
                old_name: Some(old_emote.name.clone()),
                new_name: None,
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: None,
                gap_to: None,
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            };
            apply_actor(&mut remove, actor);
            events.push(remove);

            mutations.push(CatalogMutation::Upsert {
                emote_id: new_emote.emote_id.clone(),
                name: new_emote.name.clone(),
                active: true,
            });
            let mut add = NewSetEvent {
                at,
                kind: SetEventKind::Add,
                emote_id: Some(new_emote.emote_id.clone()),
                old_name: None,
                new_name: Some(new_emote.name.clone()),
                actor_7tv_id: None,
                actor_username: None,
                actor_display_name: None,
                gap_from: None,
                gap_to: None,
                requester_login: None,
                requester_twitch_id: None,
                via: None,
                attribution_method: None,
            };
            apply_actor(&mut add, actor);
            events.push(add);
        }
    }

    SetChangeBatch {
        updated_at: at,
        mutations,
        events,
    }
}

fn apply_actor(event: &mut NewSetEvent, actor: Option<&ActorRef>) {
    if let Some(actor) = actor {
        event.actor_7tv_id = actor.id.clone();
        event.actor_username = actor.username.clone();
        event.actor_display_name = actor.display_name.clone();
    }
}

/// Open the EventAPI websocket and subscribe for live set audit + set switches.
///
/// - `emote_set.update` + `object_id` = active set (membership changes)
/// - `user.update` + `object_id` = 7TV user (active-set switches)
///
/// Heartbeat handling and reconnect logic belong in the ingest loop. Server heartbeats
/// use opcode 2 and require no client ack; Hello (opcode 1) may include `heartbeat_interval`.
pub async fn connect_and_subscribe(
    emote_set_id: &str,
    seventv_user_id: &str,
) -> Result<EventApiConnection> {
    let (ws, _) = connect_async(EVENTAPI_URL)
        .await
        .wrap_err_with(|| format!("failed to connect to EventAPI at {EVENTAPI_URL}"))?;
    let (mut sink, stream) = ws.split();
    for sub in [
        subscribe_emote_set_update_message(emote_set_id),
        subscribe_user_update_message(seventv_user_id),
    ] {
        sink.send(Message::Text(sub.into()))
            .await
            .wrap_err("failed to send EventAPI subscribe")?;
    }
    Ok(EventApiConnection { sink, stream })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emote_tracker::types::SetEventKind;

    #[test]
    fn parse_push_with_actor() {
        let raw = include_str!("fixtures/dispatch_push.json");
        let msg = parse_server_message(raw).unwrap().unwrap();
        let ParsedEvent::Dispatch(d) = msg else {
            panic!("expected dispatch");
        };
        assert_eq!(d.pushed[0].name, "Kappa");
        assert!(d.actor.as_ref().unwrap().display_name.is_some());
    }

    #[test]
    fn parse_missing_actor_still_ok() {
        let raw = include_str!("fixtures/dispatch_missing_actor.json");
        let msg = parse_server_message(raw).unwrap().unwrap();
        let ParsedEvent::Dispatch(d) = msg else {
            panic!("expected dispatch");
        };
        assert!(d.actor.is_none());
    }

    #[test]
    fn parse_pull_extracts_old_value() {
        let raw = include_str!("fixtures/dispatch_pull.json");
        let msg = parse_server_message(raw).unwrap().unwrap();
        let ParsedEvent::Dispatch(d) = msg else {
            panic!("expected dispatch");
        };
        assert_eq!(d.pulled[0].name, "GIGACHAD");
    }

    #[test]
    fn parse_rename_extracts_old_and_new() {
        let raw = include_str!("fixtures/dispatch_rename.json");
        let msg = parse_server_message(raw).unwrap().unwrap();
        let ParsedEvent::Dispatch(d) = msg else {
            panic!("expected dispatch");
        };
        let (old, new) = &d.updated[0];
        assert_eq!(old.name, "GIGACHAD");
        assert_eq!(new.name, "GigaChad");
    }

    #[test]
    fn live_change_push_maps_to_add() {
        let raw = include_str!("fixtures/dispatch_push.json");
        let ParsedEvent::Dispatch(d) = parse_server_message(raw).unwrap().unwrap() else {
            panic!("expected dispatch");
        };
        let batch = live_change_to_batch(100, &d);
        assert_eq!(batch.mutations.len(), 1);
        assert_eq!(batch.events[0].kind, SetEventKind::Add);
    }

    #[test]
    fn live_change_pull_maps_to_remove() {
        let raw = include_str!("fixtures/dispatch_pull.json");
        let ParsedEvent::Dispatch(d) = parse_server_message(raw).unwrap().unwrap() else {
            panic!("expected dispatch");
        };
        let batch = live_change_to_batch(100, &d);
        assert!(matches!(
            batch.mutations[0],
            CatalogMutation::Deactivate { .. }
        ));
        assert_eq!(batch.events[0].kind, SetEventKind::Remove);
    }

    #[test]
    fn live_change_update_maps_to_rename() {
        let raw = include_str!("fixtures/dispatch_rename.json");
        let ParsedEvent::Dispatch(d) = parse_server_message(raw).unwrap().unwrap() else {
            panic!("expected dispatch");
        };
        let batch = live_change_to_batch(100, &d);
        assert_eq!(batch.events[0].kind, SetEventKind::Rename);
    }

    #[test]
    fn parse_hello_and_heartbeat() {
        let hello = r#"{"op":1,"d":{"heartbeat_interval":25000}}"#;
        let ParsedEvent::Hello {
            heartbeat_interval_ms,
        } = parse_server_message(hello).unwrap().unwrap()
        else {
            panic!("expected hello");
        };
        assert_eq!(heartbeat_interval_ms, Some(25_000));

        let heartbeat = r#"{"op":2,"d":{}}"#;
        assert!(matches!(
            parse_server_message(heartbeat).unwrap(),
            Some(ParsedEvent::Heartbeat)
        ));
    }

    #[test]
    fn parse_end_of_stream_invalid_payload() {
        let raw = r#"{"op":7,"d":{"code":4002,"message":"Invalid Payload"}}"#;
        let ParsedEvent::EndOfStream { code, message } =
            parse_server_message(raw).unwrap().unwrap()
        else {
            panic!("expected end of stream");
        };
        assert_eq!(code, 4002);
        assert_eq!(message, "Invalid Payload");
    }

    #[test]
    fn expected_reconnect_codes_do_not_require_gap() {
        // Documented reconnect-yes close codes + observed scheduled rotation (4012).
        for code in [4000, 4006, 4007, 4008, 4012] {
            assert!(
                is_expected_reconnect(code),
                "code {code} should be an expected reconnect"
            );
        }
    }

    #[test]
    fn live_push_null_body_actor_keeps_actor_null_despite_value_actor_id() {
        // value.actor_id is the original set-adder, not the live mutation
        // performer. Copying it into actor_7tv_id would close Step A for
        // Potat-mediated null-body events and mislabel the Actor column.
        let raw = include_str!("fixtures/dispatch_push_null_actor_with_actor_id.json");
        let ParsedEvent::Dispatch(d) = parse_server_message(raw).unwrap().unwrap() else {
            panic!("expected dispatch");
        };
        assert!(d.actor.is_none());
        assert_eq!(
            d.pushed[0].actor_id.as_deref(),
            Some("01ACTORIDFROMVALUE000000")
        );
        let batch = live_change_to_batch(100, &d);
        assert!(batch.events[0].actor_7tv_id.is_none());
        assert_eq!(
            super::super::format::format_actor_display(
                batch.events[0].actor_display_name.as_deref(),
                batch.events[0].actor_username.as_deref(),
                batch.events[0].actor_7tv_id.as_deref(),
            ),
            "unknown"
        );
    }

    #[test]
    fn live_rename_null_body_actor_does_not_use_value_actor_id() {
        let raw = include_str!("fixtures/dispatch_rename_null_actor_with_actor_id.json");
        let ParsedEvent::Dispatch(d) = parse_server_message(raw).unwrap().unwrap() else {
            panic!("expected dispatch");
        };
        assert!(d.actor.is_none());
        assert!(d.updated[0].0.actor_id.is_some());
        assert!(d.updated[0].1.actor_id.is_some());
        let batch = live_change_to_batch(100, &d);
        assert!(
            batch
                .events
                .iter()
                .all(|e| e.kind != SetEventKind::Rename || e.actor_7tv_id.is_none())
        );
    }

    #[test]
    fn client_error_close_codes_are_not_expected_reconnect() {
        for code in [4001, 4002, 4003, 4004, 4009, 4010, 4011] {
            assert!(
                !is_expected_reconnect(code),
                "code {code} should not be treated as expected reconnect"
            );
        }
    }

    #[test]
    fn subscribe_emote_set_uses_object_id() {
        let raw = subscribe_emote_set_update_message("01SETID");
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["op"], 35);
        assert_eq!(value["d"]["type"], "emote_set.update");
        assert_eq!(value["d"]["condition"]["object_id"], "01SETID");
        assert!(value["d"]["condition"].get("ctx").is_none());
    }

    #[test]
    fn subscribe_user_update_uses_object_id() {
        let raw = subscribe_user_update_message("01USERID");
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["op"], 35);
        assert_eq!(value["d"]["type"], "user.update");
        assert_eq!(value["d"]["condition"]["object_id"], "01USERID");
    }

    #[test]
    fn unsubscribe_emote_set_uses_opcode_36() {
        let raw = unsubscribe_emote_set_update_message("01SETID");
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["op"], 36);
        assert_eq!(value["d"]["type"], "emote_set.update");
        assert_eq!(value["d"]["condition"]["object_id"], "01SETID");
    }

    #[test]
    fn parse_user_update_active_set_change() {
        let raw = include_str!("fixtures/dispatch_user_active_set.json");
        let msg = parse_server_message(raw).unwrap().unwrap();
        let ParsedEvent::UserActiveSetChanged(u) = msg else {
            panic!("expected user active set change, got {msg:?}");
        };
        assert_eq!(u.user_id, "01FZKKA0A8000FP66RDQP75B9A");
        assert_eq!(u.old_set_id, "01FZKKA0A8000FP66RDQP75B9A");
        assert_eq!(u.new_set_id, "01NEWACTIVESET000000000000");
        assert_eq!(u.connection_index, 0);
        assert_eq!(
            u.actor.as_ref().and_then(|a| a.display_name.as_deref()),
            Some("Streamer")
        );
    }
}
