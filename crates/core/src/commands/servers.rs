//! `!servers [name]`: online game servers on the homelab AMP panel.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, CommandContext, ExecutionClass};
use crate::{
    amp::{AmpClient, AmpDetails, AmpError, AmpServer},
    twitch::announce::format_uptime,
    util::truncate_response,
};

const MAX_REPLY_CHARS: usize = 480;

// ponytail: no cooldown and no cache, one HTTP call per use; add cooldowns.servers if it gets spammed
pub struct ServersCommand {
    amp: Arc<AmpClient>,
}

impl ServersCommand {
    pub fn new(amp: Arc<AmpClient>) -> Self {
        Self { amp }
    }

    async fn reply_for(&self, query: &str) -> String {
        let servers = match self.amp.servers().await {
            Ok(s) => s,
            Err(e) => return error_reply(e).to_owned(),
        };
        if query.is_empty() {
            return list_reply(&servers);
        }
        match pick_server(&servers, query) {
            Pick::None => not_found_reply(query),
            Pick::Many(m) => many_reply(&m),
            Pick::One(s) if !s.running => detail_reply(s, None),
            Pick::One(s) => {
                // Details are a bonus; the list data alone still answers.
                let details = self
                    .amp
                    .details(&s.id)
                    .await
                    .inspect_err(
                        |error| tracing::warn!(?error, server = %s.name, "amp details failed"),
                    )
                    .ok();
                detail_reply(s, details.as_ref())
            }
        }
    }
}

#[async_trait]
impl<T, L> Command<T, L> for ServersCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "servers"
    }

    fn name(&self) -> &str {
        "!servers"
    }

    fn description(&self) -> &str {
        "Gameserver auf dem AMP-Panel"
    }

    fn usage(&self) -> &str {
        "!servers [name]"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Background
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        ctx.cancellation.check()?;
        let reply = self.reply_for(ctx.args.join(" ").trim()).await;
        ctx.cancellation.check()?;
        ctx.sender.reply(&ctx.privmsg, reply).await;
        Ok(())
    }
}

#[derive(Debug)]
enum Pick<'a> {
    One(&'a AmpServer),
    Many(Vec<&'a AmpServer>),
    None,
}

/// Lowercase, alphanumerics only: "GregTech: New Horizon" → "gregtechnewhorizon".
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Exact match on display or instance name wins; else substring on either.
fn pick_server<'a>(servers: &'a [AmpServer], query: &str) -> Pick<'a> {
    let q = normalize(query);
    if q.is_empty() {
        return Pick::None;
    }
    let names = |s: &AmpServer| [normalize(&s.name), normalize(&s.instance_name)];
    let exact: Vec<_> = servers.iter().filter(|s| names(s).contains(&q)).collect();
    let hits = if exact.is_empty() {
        servers
            .iter()
            .filter(|s| names(s).iter().any(|n| n.contains(&q)))
            .collect()
    } else {
        exact
    };
    match hits.as_slice() {
        [] => Pick::None,
        [one] => Pick::One(one),
        _ => Pick::Many(hits),
    }
}

fn port(address: &str) -> Option<&str> {
    address.rsplit_once(':').map(|(_, p)| p)
}

fn list_reply(servers: &[AmpServer]) -> String {
    let online: Vec<String> = servers
        .iter()
        .filter(|s| s.running)
        .map(|s| {
            let parts: Vec<String> = [
                s.players.map(|(n, max)| format!("{n}/{max}")),
                s.address.as_deref().and_then(port).map(|p| format!(":{p}")),
            ]
            .into_iter()
            .flatten()
            .collect();
            if parts.is_empty() {
                s.name.clone()
            } else {
                format!("{} ({})", s.name, parts.join(", "))
            }
        })
        .collect();
    if online.is_empty() {
        return "Kein Server online".to_owned();
    }
    truncate_response(&format!("Online: {}", online.join(" · ")), MAX_REPLY_CHARS)
}

fn detail_reply(s: &AmpServer, details: Option<&AmpDetails>) -> String {
    if !s.running {
        return match &s.address {
            Some(a) => format!("{} ist offline · {a}", s.name),
            None => format!("{} ist offline", s.name),
        };
    }
    let mut head = s.name.clone();
    // Generic modules (Arma, Vintage Story) all say "GenericModule"; noise.
    if s.module != "GenericModule" && !s.module.is_empty() {
        head.push_str(&format!(" [{}]", s.module));
    }
    head.push_str(" online");
    if let Some(up) = details.and_then(|d| d.uptime) {
        head.push_str(&format!(" seit {}", format_uptime(up)));
    }
    let players = details
        .filter(|d| !d.players.is_empty())
        .map(|d| format!("Spieler: {}", d.players.join(", ")));
    let parts: Vec<String> = [
        Some(head),
        s.players.map(|(n, max)| format!("{n}/{max} Spieler")),
        s.memory_mb.map(|(n, max)| format!("RAM {n}/{max} MB")),
        s.address.clone(),
        players,
    ]
    .into_iter()
    .flatten()
    .collect();
    truncate_response(&parts.join(" · "), MAX_REPLY_CHARS)
}

fn many_reply(hits: &[&AmpServer]) -> String {
    let names: Vec<&str> = hits.iter().map(|s| s.name.as_str()).collect();
    truncate_response(
        &format!("Mehrere Treffer: {}", names.join(", ")),
        MAX_REPLY_CHARS,
    )
}

fn not_found_reply(query: &str) -> String {
    truncate_response(
        &format!("Kein Server \"{query}\" gefunden"),
        MAX_REPLY_CHARS,
    )
}

fn error_reply(e: AmpError) -> &'static str {
    match e {
        AmpError::Auth => "AMP-Login fehlgeschlagen",
        AmpError::Unavailable => "AMP gerade nicht erreichbar",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn server(name: &str, instance: &str, running: bool, addr: &str) -> AmpServer {
        AmpServer {
            id: format!("id-{instance}"),
            name: name.to_owned(),
            instance_name: instance.to_owned(),
            module: "Minecraft".to_owned(),
            running,
            players: running.then_some((0, 20)),
            memory_mb: running.then_some((9496, 10240)),
            address: Some(addr.to_owned()),
        }
    }

    fn fleet() -> Vec<AmpServer> {
        vec![
            server(
                "GregTech: New Horizon",
                "GregTechNewHorizon01",
                true,
                "192.168.196.166:25566",
            ),
            server(
                "Vintage Story",
                "VintageStory01",
                false,
                "192.168.196.166:42422",
            ),
            server(
                "Vintage Story Forever World",
                "VintageStoryForeverWorld01",
                false,
                "192.168.196.166:42420",
            ),
            server(
                "ArmaReforger",
                "ArmaReforger01",
                false,
                "192.168.196.166:2001",
            ),
            server(
                "Minecraft Vanilla 2025",
                "MinecraftVanilla202501",
                false,
                "192.168.196.166:25565",
            ),
        ]
    }

    fn one(p: Pick<'_>) -> &str {
        match p {
            Pick::One(s) => &s.name,
            other => panic!("expected One, got {other:?}"),
        }
    }

    #[test]
    fn pick_server_exact_beats_substring() {
        let f = fleet();
        assert_eq!(one(pick_server(&f, "vintage story")), "Vintage Story");
    }

    #[test]
    fn pick_server_ambiguous() {
        let f = fleet();
        match pick_server(&f, "vintage") {
            Pick::Many(m) => assert_eq!(m.len(), 2),
            other => panic!("expected Many, got {other:?}"),
        }
    }

    #[test]
    fn pick_server_unknown() {
        let f = fleet();
        assert!(matches!(pick_server(&f, "zelda"), Pick::None));
    }

    #[test]
    fn pick_server_normalizes() {
        let f = fleet();
        for q in [
            "gregtech",
            "new horizon",
            "GregTechNewHorizon",
            "gregtechnewhorizon01",
        ] {
            assert_eq!(
                one(pick_server(&f, q)),
                "GregTech: New Horizon",
                "query {q}"
            );
        }
        assert!(matches!(pick_server(&f, ":"), Pick::None));
    }

    #[test]
    fn list_reply_formats_online() {
        assert_eq!(
            list_reply(&fleet()),
            "Online: GregTech: New Horizon (0/20, :25566)"
        );
    }

    #[test]
    fn list_reply_omits_missing_pieces() {
        let mut s = server("Bare", "Bare01", true, "x");
        s.players = None;
        s.address = None;
        let mut t = server("Portonly", "P01", true, "1.2.3.4:7777");
        t.players = None;
        assert_eq!(list_reply(&[s, t]), "Online: Bare · Portonly (:7777)");
    }

    #[test]
    fn list_reply_none_online() {
        let f: Vec<_> = fleet().into_iter().filter(|s| !s.running).collect();
        assert_eq!(list_reply(&f), "Kein Server online");
    }

    #[test]
    fn list_reply_truncates() {
        let many: Vec<_> = (0..30)
            .map(|i| {
                server(
                    &format!("A very long server name number {i}"),
                    "x",
                    true,
                    "h:1",
                )
            })
            .collect();
        let reply = list_reply(&many);
        assert!(
            reply.chars().count() <= 480,
            "{} chars",
            reply.chars().count()
        );
        assert!(reply.ends_with("..."), "{reply}");
    }

    #[test]
    fn detail_reply_running_with_players() {
        let s = &fleet()[0];
        let d = AmpDetails {
            uptime: Some(Duration::from_secs(((5 * 24 + 23) * 60 + 48) * 60 + 43)),
            players: vec!["Alex".to_owned(), "Steve".to_owned()],
        };
        assert_eq!(
            detail_reply(s, Some(&d)),
            "GregTech: New Horizon [Minecraft] online seit 5d 23h 48m · 0/20 Spieler · \
             RAM 9496/10240 MB · 192.168.196.166:25566 · Spieler: Alex, Steve"
        );
    }

    #[test]
    fn detail_reply_running_without_details() {
        let s = &fleet()[0];
        assert_eq!(
            detail_reply(s, None),
            "GregTech: New Horizon [Minecraft] online · 0/20 Spieler · RAM 9496/10240 MB · \
             192.168.196.166:25566"
        );
    }

    #[test]
    fn detail_reply_hides_generic_module() {
        let mut s = fleet()[0].clone();
        s.module = "GenericModule".to_owned();
        assert!(detail_reply(&s, None).starts_with("GregTech: New Horizon online · "));
    }

    #[test]
    fn detail_reply_stopped() {
        let s = &fleet()[1];
        assert_eq!(
            detail_reply(s, None),
            "Vintage Story ist offline · 192.168.196.166:42422"
        );
    }

    #[test]
    fn many_and_not_found_copy() {
        let f = fleet();
        let Pick::Many(m) = pick_server(&f, "vintage") else {
            panic!("expected Many")
        };
        assert_eq!(
            many_reply(&m),
            "Mehrere Treffer: Vintage Story, Vintage Story Forever World"
        );
        assert_eq!(not_found_reply("zelda"), "Kein Server \"zelda\" gefunden");
    }

    #[test]
    fn error_copy() {
        assert_eq!(error_reply(AmpError::Auth), "AMP-Login fehlgeschlagen");
        assert_eq!(
            error_reply(AmpError::Unavailable),
            "AMP gerade nicht erreichbar"
        );
    }
}
