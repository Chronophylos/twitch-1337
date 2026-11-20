//! `!servers [name]`: online game servers on the homelab AMP panel.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{Command, CommandContext, ExecutionClass, servers_cooldown_duration};
use crate::{
    amp::{AmpClient, AmpDetails, AmpError, AmpServer},
    cooldown::{PerUserCooldown, format_cooldown_remaining},
    settings::SettingsHandle,
    twitch::announce::format_uptime,
    util::truncate_response,
};

const MAX_REPLY_CHARS: usize = 480;
/// Details (uptime, names) are a bonus; never hold the reply hostage for them.
const DETAILS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const NOT_FOUND_REPLY: &str = "Kein Server gefunden";

pub struct ServersCommand {
    amp: Arc<AmpClient>,
    cooldown: PerUserCooldown,
    details_timeout: std::time::Duration,
}

impl ServersCommand {
    pub fn new(amp: Arc<AmpClient>, settings: SettingsHandle) -> Self {
        Self {
            amp,
            cooldown: PerUserCooldown::live(settings, servers_cooldown_duration),
            details_timeout: DETAILS_TIMEOUT,
        }
    }

    /// Reply text, and whether it costs the viewer their cooldown. A "be more
    /// specific" or "not found" answer is free so the retry goes through.
    async fn reply_for(&self, query: &str) -> (String, bool) {
        let servers = match self.amp.servers().await {
            Ok(s) => s,
            Err(e) => return (error_reply(e).to_owned(), true),
        };
        // Invisible chars (Chatterino's U+E0000 dedupe) normalize to empty.
        if normalize(query).is_empty() {
            return (list_reply(&servers), true);
        }
        match pick_server(&servers, query) {
            Pick::None => (NOT_FOUND_REPLY.to_owned(), false),
            Pick::Many(m) => (many_reply(&m), false),
            Pick::One(s) if !s.running => (detail_reply(s, None), true),
            Pick::One(s) => {
                let details =
                    match tokio::time::timeout(self.details_timeout, self.amp.details(&s.id)).await
                    {
                        Ok(Ok(d)) => Some(d),
                        Ok(Err(error)) => {
                            tracing::warn!(?error, server = %s.name, "amp details failed");
                            None
                        }
                        Err(_) => {
                            tracing::warn!(server = %s.name, "amp details timed out");
                            None
                        }
                    };
                (detail_reply(s, details.as_ref()), true)
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
        let user = &ctx.privmsg.sender.login;
        // Charge before the AMP request so concurrent invocations can't all
        // slip past; a free answer (ambiguous, not found) refunds it.
        let stamp = match self.cooldown.try_acquire(user).await {
            Ok(stamp) => stamp,
            Err(remaining) => {
                ctx.sender
                    .reply(
                        &ctx.privmsg,
                        format!(
                            "Bitte warte noch {} Waiting",
                            format_cooldown_remaining(remaining)
                        ),
                    )
                    .await;
                return Ok(());
            }
        };
        let (reply, charged) = self.reply_for(ctx.args.join(" ").trim()).await;
        if !charged {
            self.cooldown.refund(user, stamp).await;
        }
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

fn error_reply(e: AmpError) -> &'static str {
    match e {
        AmpError::Auth => "AMP-Login fehlgeschlagen",
        AmpError::Permission => "AMP-Rechte fehlen",
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
                "192.0.2.10:25566",
            ),
            server("Vintage Story", "VintageStory01", false, "192.0.2.10:42422"),
            server(
                "Vintage Story Forever World",
                "VintageStoryForeverWorld01",
                false,
                "192.0.2.10:42420",
            ),
            server("ArmaReforger", "ArmaReforger01", false, "192.0.2.10:2001"),
            server(
                "Minecraft Vanilla 2025",
                "MinecraftVanilla202501",
                false,
                "192.0.2.10:25565",
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
             RAM 9496/10240 MB · 192.0.2.10:25566 · Spieler: Alex, Steve"
        );
    }

    #[test]
    fn detail_reply_hides_generic_module() {
        let mut s = fleet()[0].clone();
        s.module = "GenericModule".to_owned();
        assert!(detail_reply(&s, None).starts_with("GregTech: New Horizon online · "));
    }

    mod live {
        use secrecy::SecretString;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        use super::*;

        const INSTANCES: &str = include_str!("../amp/fixtures/get_instances.json");

        fn json(body: &str) -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
        }

        async fn amp_with_instances() -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/API/Core/Login"))
                .respond_with(json(r#"{"success":true,"sessionID":"s1"}"#))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/API/ADSModule/GetInstances"))
                .respond_with(json(INSTANCES))
                .mount(&server)
                .await;
            server
        }

        fn command(server: &MockServer, details_timeout: Duration) -> ServersCommand {
            crate::install_crypto_provider();
            let amp = AmpClient::with_http(
                reqwest::Client::new(),
                server.uri(),
                "bot".to_owned(),
                SecretString::new("pw".into()),
            );
            let mut cmd = ServersCommand::new(
                Arc::new(amp),
                crate::settings::SettingsHandle::from_settings(
                    crate::settings::Settings::compiled_defaults(),
                ),
            );
            cmd.details_timeout = details_timeout;
            cmd
        }

        #[tokio::test]
        async fn invisible_query_shows_the_list() {
            let server = amp_with_instances().await;
            let cmd = command(&server, Duration::from_secs(5));
            // Chatterino's duplicate-message bypass appends U+E0000.
            let (reply, charged) = cmd.reply_for("\u{E0000}").await;
            assert!(reply.starts_with("Online:"), "{reply}");
            assert!(charged);
        }

        #[tokio::test]
        async fn ambiguous_and_unknown_are_not_charged() {
            let server = amp_with_instances().await;
            let cmd = command(&server, Duration::from_secs(5));
            assert!(!cmd.reply_for("vintage").await.1);
            assert!(!cmd.reply_for("zelda").await.1);
            let (reply, charged) = cmd.reply_for("vintage story").await;
            assert!(reply.starts_with("Vintage Story ist offline"), "{reply}");
            assert!(charged);
        }

        #[tokio::test]
        async fn slow_details_fall_back_to_list_data() {
            let server = amp_with_instances().await;
            let base = "/API/ADSModule/Servers/ad13df52-a04f-4614-b6e7-9172fdd696ae/API/Core";
            Mock::given(method("POST"))
                .and(path(format!("{base}/Login")))
                .respond_with(
                    json(r#"{"success":true,"sessionID":"i1"}"#).set_delay(Duration::from_secs(3)),
                )
                .mount(&server)
                .await;
            let cmd = command(&server, Duration::from_millis(200));
            let started = std::time::Instant::now();
            let (reply, _) = cmd.reply_for("gregtech").await;
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{:?}",
                started.elapsed()
            );
            assert_eq!(
                reply,
                "GregTech: New Horizon [Minecraft] online · 0/20 Spieler · RAM 9496/10240 MB · \
                 192.0.2.10:25566"
            );
        }
    }
}
