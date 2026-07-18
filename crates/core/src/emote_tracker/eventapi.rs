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
const OP_SUBSCRIBE: u64 = 35;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedEvent {
    Hello { heartbeat_interval_ms: Option<u64> },
    Heartbeat,
    Dispatch(DispatchEmoteSetUpdate),
    Ack,
    Other { op: u64 },
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
        op => Ok(Some(ParsedEvent::Other { op })),
    }
}

fn parse_dispatch(data: Value) -> Result<Option<ParsedEvent>> {
    let event_type = data
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("dispatch missing d.type"))?;
    if event_type != "emote_set.update" {
        return Ok(None);
    }

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
    Ok(DesiredEmote { emote_id, name })
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

/// Open the EventAPI websocket and subscribe to `emote_set.update` for a Twitch channel.
///
/// Heartbeat handling and reconnect logic belong in the ingest loop (Task 5). Server heartbeats
/// use opcode 2 and require no client ack; Hello (opcode 1) may include `heartbeat_interval`.
pub async fn connect_and_subscribe(twitch_channel_id: &str) -> Result<EventApiConnection> {
    let (ws, _) = connect_async(EVENTAPI_URL)
        .await
        .wrap_err_with(|| format!("failed to connect to EventAPI at {EVENTAPI_URL}"))?;
    let (mut sink, stream) = ws.split();
    let sub = serde_json::json!({
        "op": OP_SUBSCRIBE,
        "d": {
            "type": "emote_set.update",
            "condition": {
                "channel": twitch_channel_id,
                "platform": "TWITCH"
            }
        }
    });
    sink.send(Message::Text(sub.to_string().into()))
        .await
        .wrap_err("failed to send EventAPI subscribe")?;
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
}
