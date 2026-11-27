//! Aurora's Viewer-Facing Tools and their public German documentation.
//!
//! The catalog is initialized from the same dependencies used to execute a
//! Turn. Both tool registration and the public `/commands` page then read the
//! resulting availability, so a backing integration cannot be advertised when
//! Aurora did not receive it. Model-facing prompt instructions stay in the
//! individual tool modules; [`ViewerToolInfo`] is deliberately public copy.

use std::sync::{Arc, OnceLock};

use llm::ToolDefinition;

/// Backing integrations that are optional for Aurora's Viewer-Facing Tools.
/// Tools without a field here are available on every Aurora Turn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AuroraToolAvailability {
    pub dhl: bool,
    pub web: bool,
    pub emotes: bool,
    pub emote_stats: bool,
}

impl AuroraToolAvailability {
    /// Every optional integration is available.
    pub const fn all() -> Self {
        Self {
            dhl: true,
            web: true,
            emotes: true,
            emote_stats: true,
        }
    }
}

/// Shared Aurora tool availability, published when the Turn runner is built.
///
/// Clones refer to the same cell. Before initialization (for example, when AI
/// is disabled or the bot is still composing commands) the enabled set is
/// empty. Initialization happens once with the runner's actual dependencies.
#[derive(Clone, Debug, Default)]
pub struct AuroraToolCatalog {
    availability: Arc<OnceLock<AuroraToolAvailability>>,
}

impl AuroraToolCatalog {
    /// Publish availability from Aurora's executable tool dependencies.
    /// The first runner wins; production constructs one runner per process.
    pub fn initialize(&self, availability: AuroraToolAvailability) {
        let _published = self.availability.get_or_init(|| availability);
    }

    /// Enabled Viewer-Facing Tools in stable display and registration order.
    pub fn enabled_tools(&self) -> impl Iterator<Item = ViewerTool> {
        let availability = self.availability.get().copied();
        ViewerTool::ALL
            .into_iter()
            .filter(move |tool| availability.is_some_and(|a| tool.is_enabled(a)))
    }

    /// Public metadata for the enabled Viewer-Facing Tools.
    pub fn enabled_metadata(&self) -> impl Iterator<Item = ViewerToolInfo> {
        self.enabled_tools().map(ViewerTool::metadata)
    }
}

/// Public copy for one Viewer-Facing Tool. This is not model prompt text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewerToolInfo {
    pub display_name: &'static str,
    pub description: &'static str,
    pub example: &'static str,
}

/// Every capability a viewer can ask Aurora to use. Internal Memory/Scratch
/// maintenance and command discovery intentionally have no variant here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewerTool {
    DrawTarot,
    PlanFlight,
    SearchPackages,
    LookupNetwork,
    RunBash,
    TrackShipment,
    WebSearch,
    ReadUrl,
    SearchEmotes,
    EmoteStats,
}

impl ViewerTool {
    pub const ALL: [Self; 10] = [
        Self::DrawTarot,
        Self::PlanFlight,
        Self::SearchPackages,
        Self::LookupNetwork,
        Self::RunBash,
        Self::TrackShipment,
        Self::WebSearch,
        Self::ReadUrl,
        Self::SearchEmotes,
        Self::EmoteStats,
    ];

    /// Stable model identifier, used for registration tests rather than as
    /// public invocation syntax.
    pub const fn id(self) -> &'static str {
        match self {
            Self::DrawTarot => super::tarot_tool::TAROT_TOOL_NAME,
            Self::PlanFlight => super::flight_tool::PLAN_FLIGHT_TOOL_NAME,
            Self::SearchPackages => super::packages_tool::SEARCH_PACKAGES_TOOL_NAME,
            Self::LookupNetwork => super::network_tool::LOOKUP_NETWORK_TOOL_NAME,
            Self::RunBash => super::bash_tool::RUN_BASH_TOOL_NAME,
            Self::TrackShipment => super::dhl_tool::TRACK_SHIPMENT_TOOL_NAME,
            Self::WebSearch => super::content::tools::WEB_SEARCH_TOOL_NAME,
            Self::ReadUrl => super::content::tools::READ_URL_TOOL_NAME,
            Self::SearchEmotes => super::emote_tool::SEARCH_EMOTES_TOOL_NAME,
            Self::EmoteStats => super::emote_stats_tool::EMOTE_STATS_TOOL_NAME,
        }
    }

    /// Friendly public copy, kept separate from the operational prompt text.
    pub const fn metadata(self) -> ViewerToolInfo {
        let (display_name, description, example) = match self {
            Self::DrawTarot => (
                "Tarotkarten",
                "Zieht eine Tageskarte oder ein Tarotbild zu einer Frage.",
                "!ai Zieh eine Tarot-Tageskarte für mich.",
            ),
            Self::PlanFlight => (
                "Flugplaner",
                "Plant eine Simulatorroute mit Flughäfen, Flugzeug und SimBrief-Link.",
                "!ai Plane einen zweistündigen Flug ab Frankfurt.",
            ),
            Self::SearchPackages => (
                "Paketsuche",
                "Sucht Softwarepakete in Repositories wie AUR, Debian, Ubuntu und winget.",
                "!ai Welche Version von ripgrep ist in Debian?",
            ),
            Self::LookupNetwork => (
                "Netzwerk-Lookup",
                "Schlägt öffentliche IPs, AS-Nummern und Domains in Registry-Daten nach.",
                "!ai Schlag die Domain twitch.tv nach.",
            ),
            Self::RunBash => (
                "Bash-Sandbox",
                "Führt kleine Bash-Skripte ohne Netz- oder Host-Zugriff in einer wegwerfbaren Sandbox aus.",
                "!ai Rechne mit der Bash-Sandbox die ersten zehn Fibonacci-Zahlen aus.",
            ),
            Self::TrackShipment => (
                "DHL-Sendung",
                "Fragt Status und letzte Stationen einer DHL-Sendung ab.",
                "!ai Wo ist mein DHL-Paket mit der Sendungsnummer 00340434161094042557?",
            ),
            Self::WebSearch => (
                "Websuche",
                "Findet aktuelle Informationen und passende Quellen im Web.",
                "!ai Such nach dem aktuellen Rust-Release und nenne mir die Quelle.",
            ),
            Self::ReadUrl => (
                "URL lesen",
                "Öffnet einen Link und fasst Webseiten, Dokumente oder Medien zusammen.",
                "!ai Fass https://www.rust-lang.org/de für den Chat zusammen.",
            ),
            Self::SearchEmotes => (
                "Emote-Suche",
                "Findet passende 7TV-Emotes nach Code, Aussehen oder Bedeutung.",
                "!ai Welches Channel-Emote passt zu sarkastischem Applaus?",
            ),
            Self::EmoteStats => (
                "Emote-Statistik",
                "Zeigt Nutzungsranglisten und Set-Änderungen der Channel-Emotes.",
                "!ai Welche fünf Emotes wurden diese Woche am häufigsten genutzt?",
            ),
        };
        ViewerToolInfo {
            display_name,
            description,
            example,
        }
    }

    /// The single conditional-availability decision shared by execution and
    /// public documentation.
    const fn is_enabled(self, availability: AuroraToolAvailability) -> bool {
        match self {
            Self::TrackShipment => availability.dhl,
            Self::WebSearch | Self::ReadUrl => availability.web,
            Self::SearchEmotes => availability.emotes,
            Self::EmoteStats => availability.emote_stats,
            Self::DrawTarot
            | Self::PlanFlight
            | Self::SearchPackages
            | Self::LookupNetwork
            | Self::RunBash => true,
        }
    }

    /// Aurora-only tools are omitted from Grok Turns. Shared tools use the
    /// same integration availability but have no separate public Grok entry.
    pub(crate) const fn is_aurora_only(self) -> bool {
        matches!(
            self,
            Self::DrawTarot
                | Self::PlanFlight
                | Self::SearchPackages
                | Self::LookupNetwork
                | Self::RunBash
                | Self::TrackShipment
        )
    }

    /// Preserve the existing model-facing definitions and schemas.
    pub(crate) fn definition(self) -> ToolDefinition {
        match self {
            Self::DrawTarot => super::tarot_tool::tarot_tool(),
            Self::PlanFlight => super::flight_tool::plan_flight_tool(),
            Self::SearchPackages => super::packages_tool::search_packages_tool(),
            Self::LookupNetwork => super::network_tool::lookup_network_tool(),
            Self::RunBash => super::bash_tool::run_bash_tool(),
            Self::TrackShipment => super::dhl_tool::track_shipment_tool(),
            Self::WebSearch => super::content::tools::web_search_tool(),
            Self::ReadUrl => super::content::tools::read_url_tool(),
            Self::SearchEmotes => super::emote_tool::search_emotes_tool(),
            Self::EmoteStats => super::emote_stats_tool::emote_stats_tool(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn catalog(availability: AuroraToolAvailability) -> AuroraToolCatalog {
        let catalog = AuroraToolCatalog::default();
        catalog.initialize(availability);
        catalog
    }

    fn ids(catalog: &AuroraToolCatalog) -> Vec<&'static str> {
        catalog.enabled_tools().map(ViewerTool::id).collect()
    }

    #[test]
    fn uninitialized_catalog_has_no_enabled_tools() {
        let catalog = AuroraToolCatalog::default();
        assert_eq!(catalog.enabled_tools().count(), 0);
        assert_eq!(catalog.enabled_metadata().count(), 0);
    }

    #[test]
    fn full_catalog_lists_every_viewer_tool_once_with_public_copy() {
        let catalog = catalog(AuroraToolAvailability::all());
        let actual = ids(&catalog);
        let expected = [
            "draw_tarot",
            "plan_flight",
            "search_packages",
            "lookup_network",
            "run_bash",
            "track_shipment",
            "web_search",
            "read_url",
            "search_emotes",
            "emote_stats",
        ];
        assert_eq!(actual, expected);
        assert_eq!(actual.iter().copied().collect::<BTreeSet<_>>().len(), 10);

        for tool in ViewerTool::ALL {
            let info = tool.metadata();
            assert!(!info.display_name.trim().is_empty());
            assert_ne!(info.display_name, tool.id());
            assert!(!info.description.trim().is_empty());
            assert!(!info.description.contains('\n'));
            assert!(info.example.starts_with("!ai "), "{info:?}");
            assert!(info.example.len() > "!ai ".len());
            assert_eq!(tool.definition().name, tool.id());
            assert_ne!(tool.definition().description, info.description);
        }
    }

    #[test]
    fn optional_integrations_control_exactly_their_own_tools() {
        let baseline = vec![
            "draw_tarot",
            "plan_flight",
            "search_packages",
            "lookup_network",
            "run_bash",
        ];
        assert_eq!(ids(&catalog(AuroraToolAvailability::default())), baseline);

        let cases = [
            (
                AuroraToolAvailability {
                    dhl: true,
                    ..Default::default()
                },
                vec!["track_shipment"],
            ),
            (
                AuroraToolAvailability {
                    web: true,
                    ..Default::default()
                },
                vec!["web_search", "read_url"],
            ),
            (
                AuroraToolAvailability {
                    emotes: true,
                    ..Default::default()
                },
                vec!["search_emotes"],
            ),
            (
                AuroraToolAvailability {
                    emote_stats: true,
                    ..Default::default()
                },
                vec!["emote_stats"],
            ),
        ];
        for (availability, additions) in cases {
            let mut expected = baseline.clone();
            expected.extend(additions);
            assert_eq!(ids(&catalog(availability)), expected);
        }
    }

    #[test]
    fn internal_memory_scratch_and_discovery_tools_are_never_public() {
        let internal: BTreeSet<String> = super::super::memory::tools::chat_turn_tools()
            .into_iter()
            .chain(super::super::memory::tools::grok_chat_turn_tools())
            .chain(std::iter::once(
                super::super::commands_tool::list_commands_tool(),
            ))
            .map(|tool| tool.name)
            .collect();
        assert!(internal.contains("list_commands"));

        for tool in ViewerTool::ALL {
            assert!(
                !internal.contains(tool.id()),
                "{} is internal but entered the Viewer-Facing inventory",
                tool.id()
            );
        }
    }
}
