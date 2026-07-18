use std::collections::HashMap;

use super::types::{CatalogMutation, EmoteRow, NewSetEvent, SetChangeBatch, SetEventKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredEmote {
    pub emote_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorRef {
    pub id: Option<String>,
    pub username: Option<String>,
    pub display_name: Option<String>,
}

pub fn bootstrap_batch(at: i64, desired: &[DesiredEmote]) -> SetChangeBatch {
    let mut mutations = Vec::with_capacity(desired.len());
    let mut events = Vec::with_capacity(desired.len());

    for emote in desired {
        mutations.push(CatalogMutation::Upsert {
            emote_id: emote.emote_id.clone(),
            name: emote.name.clone(),
            active: true,
        });
        events.push(NewSetEvent {
            at,
            kind: SetEventKind::Bootstrap,
            emote_id: Some(emote.emote_id.clone()),
            old_name: None,
            new_name: Some(emote.name.clone()),
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: None,
            gap_to: None,
        });
    }

    SetChangeBatch {
        updated_at: at,
        mutations,
        events,
    }
}

pub fn reconcile_batch(
    at: i64,
    current_active: &[EmoteRow],
    desired: &[DesiredEmote],
    gap: Option<(i64, i64)>,
    actor: Option<&ActorRef>,
) -> SetChangeBatch {
    let current: HashMap<&str, &EmoteRow> = current_active
        .iter()
        .filter(|row| row.active)
        .map(|row| (row.emote_id.as_str(), row))
        .collect();
    let desired_map: HashMap<&str, &DesiredEmote> = desired
        .iter()
        .map(|emote| (emote.emote_id.as_str(), emote))
        .collect();

    let mut mutations = Vec::new();
    let mut events = Vec::new();

    for (&emote_id, desired_emote) in &desired_map {
        match current.get(emote_id) {
            None => {
                mutations.push(CatalogMutation::Upsert {
                    emote_id: desired_emote.emote_id.clone(),
                    name: desired_emote.name.clone(),
                    active: true,
                });
                let mut event = NewSetEvent {
                    at,
                    kind: SetEventKind::Add,
                    emote_id: Some(desired_emote.emote_id.clone()),
                    old_name: None,
                    new_name: Some(desired_emote.name.clone()),
                    actor_7tv_id: None,
                    actor_username: None,
                    actor_display_name: None,
                    gap_from: None,
                    gap_to: None,
                };
                apply_actor(&mut event, actor);
                events.push(event);
            }
            Some(current_row) if current_row.name != desired_emote.name => {
                mutations.push(CatalogMutation::Upsert {
                    emote_id: desired_emote.emote_id.clone(),
                    name: desired_emote.name.clone(),
                    active: true,
                });
                let mut event = NewSetEvent {
                    at,
                    kind: SetEventKind::Rename,
                    emote_id: Some(desired_emote.emote_id.clone()),
                    old_name: Some(current_row.name.clone()),
                    new_name: Some(desired_emote.name.clone()),
                    actor_7tv_id: None,
                    actor_username: None,
                    actor_display_name: None,
                    gap_from: None,
                    gap_to: None,
                };
                apply_actor(&mut event, actor);
                events.push(event);
            }
            Some(_) => {}
        }
    }

    for (&emote_id, current_row) in &current {
        if !desired_map.contains_key(emote_id) {
            mutations.push(CatalogMutation::Deactivate {
                emote_id: current_row.emote_id.clone(),
            });
            let mut event = NewSetEvent {
                at,
                kind: SetEventKind::Remove,
                emote_id: Some(current_row.emote_id.clone()),
                old_name: Some(current_row.name.clone()),
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
    }

    if let Some((gap_from, gap_to)) = gap {
        events.push(NewSetEvent {
            at,
            kind: SetEventKind::Gap,
            emote_id: None,
            old_name: None,
            new_name: None,
            actor_7tv_id: None,
            actor_username: None,
            actor_display_name: None,
            gap_from: Some(gap_from),
            gap_to: Some(gap_to),
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_writes_one_event_per_emote_no_gap() {
        let batch = bootstrap_batch(
            100,
            &[DesiredEmote {
                emote_id: "a".into(),
                name: "A".into(),
            }],
        );
        assert_eq!(batch.mutations.len(), 1);
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].kind, SetEventKind::Bootstrap);
        assert!(batch.events.iter().all(|e| e.kind != SetEventKind::Gap));
    }

    #[test]
    fn reconcile_add_remove_rename_and_optional_gap() {
        let current = vec![
            EmoteRow {
                emote_id: "keep".into(),
                name: "Old".into(),
                active: true,
                updated_at: 1,
            },
            EmoteRow {
                emote_id: "gone".into(),
                name: "Gone".into(),
                active: true,
                updated_at: 1,
            },
        ];
        let desired = vec![
            DesiredEmote {
                emote_id: "keep".into(),
                name: "New".into(),
            },
            DesiredEmote {
                emote_id: "fresh".into(),
                name: "Fresh".into(),
            },
        ];
        let batch = reconcile_batch(50, &current, &desired, Some((40, 50)), None);
        assert!(batch.events.iter().any(|e| e.kind == SetEventKind::Rename));
        assert!(batch.events.iter().any(|e| e.kind == SetEventKind::Remove));
        assert!(batch.events.iter().any(|e| e.kind == SetEventKind::Add));
        let gap = batch
            .events
            .iter()
            .find(|e| e.kind == SetEventKind::Gap)
            .unwrap();
        assert_eq!(gap.gap_from, Some(40));
        assert_eq!(gap.gap_to, Some(50));
    }
}
