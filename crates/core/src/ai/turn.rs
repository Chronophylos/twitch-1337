//! The Turn: one agent invocation — a fresh completion carrying Memory,
//! Recent Chat and this instruction (see CONTEXT.md).
//!
//! [`TurnRunner::run_turn`] owns profile resolution, substitution, recap
//! gating, memory injection, the agent loop and reply shaping. It never
//! touches IRC: it takes a [`TurnInput`] and returns a [`TurnOutcome`], so
//! prompt assembly is testable against a fake [`llm::LlmClient`] without a
//! Twitch round-trip. `AiCommand` is the Twitch adapter around it.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use eyre::Result;
use tracing::{error, warn};

use llm::{
    AgentOpts, AgentOutcome, LlmError, ToolCall, ToolCallRound, ToolChatCompletionRequest,
    ToolExecutor, ToolResultMessage, TraceIds, run_agent,
};

use crate::ai::command::{AiCommandDeps, AiMemoryV2, AiWeb, ChatContext};
use crate::ai::content;
use crate::ai::memory::inject;
use crate::ai::memory::tools::{
    ChatTurnExecutor, ChatTurnExecutorOpts, chat_turn_tools, grok_chat_turn_tools,
};
use crate::ai::memory::types::Role;
use crate::ai::model_catalog::ModelCatalog;
use crate::ai::recap::{RECAP_SHEET_LOGINS, RecapChannel, RecapHandle};
use crate::ai::reply::{ReplyLimits, RewordOpts, prepare_agent_reply};
use crate::commands::CommandCancellation;
use crate::settings::SettingsHandle;
use crate::twitch::mouth::Agent;
use crate::twitch::seventv::SevenTvEmoteProvider;

/// Sent to the invoking viewer when a round exceeds the profile timeout.
pub(crate) const TIMEOUT_NOTICE: &str = "hat zu hart gedacht, nochmal probieren Stare";

/// The preset's name for an agent. `Utility` speaks as Aurora.
fn agent_key(agent: Agent) -> &'static str {
    match agent {
        Agent::Grok => "grok",
        Agent::Aurora | Agent::Utility => "aurora",
    }
}

/// Who is speaking this turn.
pub struct TurnSpeaker {
    pub login: String,
    /// Raw Twitch display name. Empty when Twitch sent none; the login is
    /// substituted for the prompt but the raw value still reaches the
    /// memory executor, which is what a user sheet records.
    pub display_name: String,
    pub user_id: String,
    pub role: Role,
}

/// The message this turn replies to, when the invocation was a Twitch reply.
pub struct TurnReplyParent {
    pub user_id: String,
    pub login: String,
    pub text: String,
}

/// Everything the turn needs from the invocation. No `twitch_irc` types: the
/// dreamer and any future agent have no privmsg to hand over.
pub struct TurnInput {
    pub agent: Agent,
    pub instruction: String,
    pub speaker: TurnSpeaker,
    pub channel_login: String,
    pub channel_id: String,
    pub reply_parent: Option<TurnReplyParent>,
    pub cancellation: CommandCancellation,
}

/// What the turn produced. Transport-free — the caller decides how any of
/// it reaches chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The agent's answer, plus the persona it should be recorded under.
    Reply { text: String, display_name: String },
    /// A short notice for the invoking viewer, sent over plain IRC rather
    /// than the agent's mouth.
    Notice(&'static str),
    /// Nothing to say. Covers a missing profile, an empty model, the round
    /// cap, and a whitespace-only answer; the cause is in the logs.
    Silent,
}

/// Owns every dependency the turn body needs, which is all of
/// [`AiCommandDeps`] — the adapter keeps only the cooldown.
pub struct TurnRunner {
    registry: Arc<crate::llm_factory::LlmRegistry>,
    settings: SettingsHandle,
    // ponytail: crate-visible because `AiCommand` reads these five for the
    // speak/record tail. Give them accessors if a third caller shows up.
    pub(crate) chat_ctx: Option<ChatContext>,
    pub(crate) memory: AiMemoryV2,
    web: Option<AiWeb>,
    emotes: Option<Arc<SevenTvEmoteProvider>>,
    pub(crate) bot_username: String,
    doener: Arc<crate::doener::DoeneratlasClient>,
    repology: Arc<crate::repology::RepologyClient>,
    pronouns: Arc<crate::pronouns::PronounsClient>,
    model_catalog: Arc<ModelCatalog>,
    pub(crate) mouths: Arc<crate::twitch::mouth::MouthRouter>,
    pub(crate) echo: Arc<crate::twitch::echo::EchoGuard>,
    dhl: Option<Arc<crate::dhl::DhlClient>>,
    emote_tracker: Option<Arc<crate::emote_tracker::EmoteTrackerHandle>>,
    recap: Option<Arc<RecapHandle>>,
    langfuse: Option<Arc<crate::ai::langfuse::LangfuseHandle>>,
    /// Per-process nonce stamped into memory fences. Stable across
    /// turns so the memory block is a byte-stable cache prefix; the real
    /// fence-injection guard is `scrub_for_inject`, not nonce freshness.
    prompt_nonce: String,
}

impl TurnRunner {
    pub fn new(deps: AiCommandDeps) -> Self {
        Self {
            registry: deps.registry,
            settings: deps.settings,
            chat_ctx: deps.chat_ctx,
            memory: deps.memory,
            web: deps.web,
            emotes: deps.emotes,
            bot_username: deps.bot_username,
            doener: deps.doener,
            repology: deps.repology,
            pronouns: deps.pronouns,
            model_catalog: deps.model_catalog,
            mouths: deps.mouths,
            echo: deps.echo,
            dhl: deps.dhl,
            emote_tracker: deps.emote_tracker,
            recap: deps.recap,
            langfuse: deps.langfuse,
            prompt_nonce: inject::fresh_nonce(),
        }
    }

    /// Run one agent turn: resolve the profile, assemble the prompt, drive
    /// the agent loop, shape the reply.
    pub async fn run_turn(&self, input: TurnInput) -> Result<TurnOutcome> {
        input.cancellation.check()?;
        // Which logical agent is speaking this turn. Drives chat-history
        // `(self)` tagging, reply-parent copy, memory posture, and the
        // Konten/Accounts system appendix.
        let agent = input.agent;
        let grok_alias = agent == Agent::Grok;

        // Snapshot connection knobs once per turn so dashboard edits take
        // effect on the next invocation without a bot restart.
        let snap = self.settings.load();
        // Each agent resolves its own profile at turn time.
        let profile_name = if grok_alias {
            &snap.ai.grok.profile
        } else {
            &snap.ai.aurora.profile
        };
        let Some((llm_client, connection)) = self.registry.resolve_named(profile_name) else {
            // Log only: a config problem is not the chatter's business.
            error!("AI command skipped, no usable LLM profile");
            return Ok(TurnOutcome::Silent);
        };
        let turn_timeout = Duration::from_secs(connection.timeout);
        let model = connection.model.clone();
        let reasoning_effort = connection.reasoning_effort.clone();
        let service_tier = connection.service_tier.clone();
        if model.trim().is_empty() {
            // Log only: a config problem is not the chatter's business.
            error!("AI command skipped, model is empty in settings");
            return Ok(TurnOutcome::Silent);
        }
        let aurora_persona = snap.ai.aurora.persona_name.clone();
        let grok_persona = snap.ai.grok.persona_name.clone();
        let display_name = if grok_alias {
            grok_persona.clone()
        } else {
            aurora_persona.clone()
        };
        let reply_target = snap.ai.aurora.reply_target_chars;
        let reply_hard = snap.ai.aurora.reply_hard_chars;
        let reply_limits = if grok_alias {
            ReplyLimits::GROK
        } else {
            ReplyLimits {
                soft: snap.ai.aurora.reply_soft_chars,
                hard: snap.ai.aurora.reply_hard_chars,
            }
        };
        let history = snap.ai.history.clone();
        let recap = snap.ai.recap.clone();
        drop(snap);

        input.cancellation.check()?;
        let model_display = self.model_catalog.display_name(&connection, &model).await;
        input.cancellation.check()?;

        let mem = &self.memory;
        let cc = self.chat_ctx.as_ref();
        let role = input.speaker.role;
        let now_berlin = Utc::now()
            .with_timezone(&chrono_tz::Europe::Berlin)
            .format("%Y-%m-%d")
            .to_string();

        let sender_display = if input.speaker.display_name.is_empty() {
            input.speaker.login.as_str()
        } else {
            input.speaker.display_name.as_str()
        };
        let sender_user_id = input.speaker.user_id.as_str();
        let invocation_channel = if cc.is_some_and(|c| c.is_ai_channel(&input.channel_login)) {
            inject::InvocationChannel::AiChannel
        } else {
            inject::InvocationChannel::Primary
        };
        let recap_channel = match invocation_channel {
            inject::InvocationChannel::Primary => RecapChannel::Primary,
            inject::InvocationChannel::AiChannel => RecapChannel::AiChannel,
        };
        let recap_profile = recap.profile.as_deref();
        let recap_parked = if let Some(ref handle) = self.recap {
            handle.parked().await
        } else {
            true
        };
        let recap_enabled = recap_enabled_for_profile(recap_profile, recap_parked);
        let (primary_through_seq, ai_through_seq) = if let Some(ref handle) = self.recap {
            (
                handle.primary_through_seq().await,
                handle.ai_through_seq().await,
            )
        } else {
            (0, 0)
        };
        let recap_sheet_logins = if recap_profile_is_set(recap_profile) {
            if let Some(ref handle) = self.recap {
                union_recap_sheet_logins(handle, recap_channel).await
            } else {
                BTreeSet::new()
            }
        } else {
            BTreeSet::new()
        };
        let recap_suffix = if recap_profile_is_set(recap_profile) {
            if let Some(ref handle) = self.recap {
                let primary_login = cc
                    .map(|c| c.primary_login.as_str())
                    .unwrap_or(input.channel_login.as_str());
                let ai_channel_login = cc
                    .and_then(|c| c.ai_channel_login.as_deref())
                    .unwrap_or(primary_login);
                handle
                    .render_suffix(
                        recap_channel,
                        primary_login,
                        ai_channel_login,
                        recap.max_notes as usize,
                    )
                    .await
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        // Memory (SOUL/LORE/users) goes in the system message so the
        // prompt cache survives across turns. Scratch and recent chat
        // go in the user message: any scratch write or chat line would otherwise
        // invalidate the system-message cache every turn.
        let build_opts = inject::BuildOpts {
            inject_byte_budget: mem.inject_byte_budget,
            // Per-process nonce: memory only changes bytes when its
            // content actually changes, so the prefix caches across turns.
            nonce: self.prompt_nonce.clone(),
            primary_history: cc.map(|c| c.primary_history.clone()),
            primary_login: cc
                .map(|c| c.primary_login.clone())
                .unwrap_or_else(|| input.channel_login.clone()),
            ai_channel_history: cc.and_then(|c| c.ai_channel_history.clone()),
            ai_channel_login: cc.and_then(|c| c.ai_channel_login.clone()),
            invocation_channel,
            current_agent: agent,
            // Every login the bot speaks under. Only used to rescue `(self)` on
            // prefilled rows, which carry no agent tag.
            self_logins: std::iter::once(self.bot_username.to_ascii_lowercase())
                .chain(self.echo.mouth_logins().iter().cloned())
                .collect(),
            speaker_login: input.speaker.login.clone(),
            recap_sheet_logins,
            n: history.n as usize,
            ai_channel_n: history.ai_channel_n as usize,
            batch_size: recap.batch_size as usize,
            recap_through_seq: primary_through_seq,
            ai_channel_recap_through_seq: ai_through_seq,
            recap_enabled,
        };

        let scoring_chat = {
            let buf = match invocation_channel {
                inject::InvocationChannel::Primary => build_opts.primary_history.as_ref(),
                inject::InvocationChannel::AiChannel => build_opts.ai_channel_history.as_ref(),
            };
            if let Some(buf) = buf {
                buf.lock()
                    .await
                    .snapshot()
                    .iter()
                    .map(|e| inject::format_entry_line(e, agent, &build_opts.self_logins))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                String::new()
            }
        };

        // Aurora gets bodies injected; grok gets an index and pulls what it
        // needs. Two agents, two memory postures.
        input.cancellation.check()?;
        let (recent_chat, memory, scratch, readable_paths) = if grok_alias {
            let g = inject::build_grok_turn_context(
                &mem.store,
                inject::ManifestOpts {
                    build: build_opts,
                    speaker_user_id: input.speaker.user_id.clone(),
                    reply_target_user_id: input.reply_parent.as_ref().map(|p| p.user_id.clone()),
                },
            )
            .await?;
            (g.recent_chat, g.manifest, String::new(), g.readable_paths)
        } else {
            let c = inject::build_chat_turn_context(&mem.store, build_opts).await?;
            (c.recent_chat, c.memory, c.scratch, BTreeSet::new())
        };

        let preset = crate::ai::preset::preset();
        let reply_to = input.reply_parent.as_ref().map(|parent| {
            let parent_agent = self
                .echo
                .agent_for(&parent.user_id, &parent.login)
                .filter(|a| *a != Agent::Utility)
                .map(agent_key);
            crate::ai::preset::ReplyParent {
                login: parent.login.as_str(),
                text: parent.text.as_str(),
                agent: parent_agent,
            }
        });
        let mut ctx = crate::ai::preset::PromptContext {
            agent: agent_key(agent),
            channel: &input.channel_login,
            date: &now_berlin,
            speaker: crate::ai::preset::Speaker {
                login: &input.speaker.login,
                display: sender_display,
                user_id: sender_user_id,
                role: role.as_str(),
            },
            message: crate::ai::preset::TriggerMessage {
                text: &input.instruction,
                reply_to,
            },
            persona: crate::ai::preset::Persona {
                aurora: &aurora_persona,
                grok: &grok_persona,
                aurora_login: self.mouths.login_for(Agent::Aurora),
                grok_login: self.mouths.login_for(Agent::Grok),
                utility_login: &self.bot_username,
            },
            limits: crate::ai::preset::Limits {
                reply_target,
                reply_soft: reply_limits.soft,
                reply_hard,
            },
            model: crate::ai::preset::ModelInfo {
                display: &model_display,
                id: &model,
            },
            tools: crate::ai::preset::Tools {
                emotes: self.emotes.is_some(),
                emote_stats: self.emote_tracker.is_some(),
                web: self.web.is_some(),
                dhl: self.dhl.is_some(),
            },
            inject: crate::ai::preset::Inject {
                memory: &memory,
                recap: &recap_suffix,
                scratch: &scratch,
                recent_chat: &recent_chat,
                emote_block: "",
            },
        };

        // The trigger block carries the reply-context prose, and emote scoring
        // plus grok's forced web search want the same text the model sees. Ask
        // the preset for it once rather than keeping a second formatter around.
        let instruction_for_prompt = preset.render_block("trigger", &ctx)?;

        // Volatile: the relevant-emotes block for THIS turn. It varies per turn,
        // so it lands in the user message, never the cached system prefix.
        let emote_block = if let Some(ref emotes) = self.emotes {
            emotes
                .prompt_block_for_turn(&instruction_for_prompt, &scoring_chat)
                .await
                .unwrap_or_default()
        } else {
            String::new()
        };
        ctx.inject.emote_block = &emote_block;

        let messages = preset.render(&ctx)?;

        // Identity for anyone whose sheet the model might write this turn.
        // Last writer wins per id, so a rename lands on the newest casing.
        let mut known_identities: HashMap<String, (String, String)> = HashMap::new();
        for h in [
            cc.map(|c| &c.primary_history),
            cc.and_then(|c| c.ai_channel_history.as_ref()),
        ]
        .into_iter()
        .flatten()
        {
            for e in h.lock().await.snapshot() {
                if let Some(id) = e.user_id {
                    let display = e.display_name.unwrap_or_else(|| e.username.clone());
                    known_identities.insert(id, (e.username, display));
                }
            }
        }

        let exec = ChatTurnExecutor::new(ChatTurnExecutorOpts {
            store: mem.store.clone(),
            speaker_user_id: input.speaker.user_id.clone(),
            speaker_login: input.speaker.login.clone(),
            speaker_display_name: input.speaker.display_name.clone(),
            speaker_role: role,
            // Grok has no write tools registered; a zero quota is the backstop
            // that denies a hallucinated write_file call too.
            max_writes_per_turn: if grok_alias {
                0
            } else {
                mem.max_writes_per_turn
            },
            readable_paths,
            inject_nonce: self.prompt_nonce.clone(),
            known_identities,
            pronouns: Some(self.pronouns.clone()),
            // Existing sheets stay writable even when the subject is not in
            // this turn's chat window; an id that is in neither is invented.
            writable_user_ids: mem.store.list_user_ids().await.unwrap_or_default(),
        });

        let mut tools = if grok_alias {
            grok_chat_turn_tools()
        } else {
            chat_turn_tools()
        };
        tools.push(crate::ai::doener_tool::doener_tool());
        if !grok_alias {
            tools.push(crate::ai::tarot_tool::tarot_tool());
            tools.push(crate::ai::flight_tool::plan_flight_tool());
            tools.push(crate::ai::packages_tool::search_packages_tool());
            if self.dhl.is_some() {
                tools.push(crate::ai::dhl_tool::track_shipment_tool());
            }
        }
        if self.web.is_some() {
            tools.extend(content::ai_tools());
        }
        if self.emotes.is_some() {
            tools.push(crate::ai::emote_tool::search_emotes_tool());
        }
        // Never registered without the tracker; available to Aurora and Grok
        // alike, matching search_emotes.
        if self.emote_tracker.is_some() {
            tools.push(crate::ai::emote_stats_tool::emote_stats_tool());
        }
        let trace = TraceIds {
            user: Some(input.speaker.login.clone()),
            session_id: Some(crate::ai::session::new_session_id()),
        };
        let turn_name = if grok_alias {
            "grok-turn"
        } else {
            "aurora-turn"
        };
        let outcome = llm::with_optional_turn(
            crate::ai::langfuse::LangfuseHandle::observer_from(&self.langfuse),
            llm::TurnParams {
                name: turn_name.to_string(),
                user: trace.user.clone(),
                session_id: trace.session_id.clone(),
                tags: vec![turn_name.to_string(), connection.name.clone()],
            },
            async {
                if input.cancellation.is_cancelled() {
                    return TurnOutcome::Silent;
                }
                let prior_rounds = if grok_alias && let Some(ref w) = self.web {
                    vec![forced_web_search_round(w, &instruction_for_prompt, &trace).await]
                } else {
                    Vec::new()
                };
                let req = ToolChatCompletionRequest {
                    model: model.clone(),
                    messages: messages.clone(),
                    tools,
                    reasoning_effort: reasoning_effort.clone(),
                    service_tier: service_tier.clone(),
                    prior_rounds,
                    trace: trace.clone(),
                };
                let opts = AgentOpts {
                    max_rounds: mem.max_turn_rounds,
                    per_round_timeout: Some(turn_timeout),
                };

                let combined_exec = V2Executor {
                    chat: &exec,
                    web: self.web.as_ref().map(|w| w.executor.as_ref()),
                    emotes: self.emotes.as_deref(),
                    doener: self.doener.as_ref(),
                    repology: self.repology.as_ref(),
                    dhl: self.dhl.as_deref(),
                    emote_tracker: self.emote_tracker.as_ref(),
                    trace: &trace,
                    cancellation: &input.cancellation,
                };
                if input.cancellation.is_cancelled() {
                    return TurnOutcome::Silent;
                }
                let final_text = match run_agent(&*llm_client, req, &combined_exec, opts).await {
                    Ok(AgentOutcome::Text(text)) => Ok(Some(text)),
                    Ok(AgentOutcome::MaxRoundsExceeded) => {
                        warn!("AI max_turn_rounds exceeded");
                        Ok(None)
                    }
                    Ok(AgentOutcome::Timeout { round }) => {
                        warn!(
                            round,
                            timeout_s = turn_timeout.as_secs(),
                            model = %model,
                            "AI per-round timeout"
                        );
                        Err(TIMEOUT_NOTICE)
                    }
                    Err(e) => {
                        error!(error = ?e, "AI llm error");
                        match user_facing_provider_message(&e) {
                            Some(reply) => Err(reply),
                            None => Ok(None),
                        }
                    }
                };

                match final_text {
                    Ok(Some(text)) => {
                        if input.cancellation.is_cancelled() {
                            return TurnOutcome::Silent;
                        }
                        let line = prepare_agent_reply(
                            &*llm_client,
                            &text,
                            &RewordOpts {
                                model: model.clone(),
                                timeout: turn_timeout,
                                reasoning_effort: reasoning_effort.clone(),
                                service_tier: service_tier.clone(),
                                trace: trace.clone(),
                            },
                            &reply_limits,
                        )
                        .await;
                        if line.is_empty() {
                            TurnOutcome::Silent
                        } else {
                            TurnOutcome::Reply {
                                text: line,
                                display_name: display_name.clone(),
                            }
                        }
                    }
                    Ok(None) => TurnOutcome::Silent,
                    Err(notice) => TurnOutcome::Notice(notice),
                }
            },
        )
        .await;

        if input.cancellation.is_cancelled() {
            return Err(crate::commands::CommandCancelled.into());
        }
        Ok(outcome)
    }
}

/// Memory-v2 path executor that dispatches by tool name to the chat-turn
/// executor (write_file/write_scratch/delete_scratch), the web search executor
/// (web_search/read_url) when web tools are configured, the on-demand
/// `search_emotes` tool when the emote provider is active, the always-on
/// doener_index tool, the Aurora-only draw_tarot and plan_flight tools, or the
/// Aurora-only search_packages tool.
struct V2Executor<'a> {
    chat: &'a ChatTurnExecutor,
    web: Option<&'a content::ContentToolExecutor>,
    emotes: Option<&'a SevenTvEmoteProvider>,
    doener: &'a crate::doener::DoeneratlasClient,
    repology: &'a crate::repology::RepologyClient,
    dhl: Option<&'a crate::dhl::DhlClient>,
    emote_tracker: Option<&'a Arc<crate::emote_tracker::EmoteTrackerHandle>>,
    trace: &'a TraceIds,
    cancellation: &'a CommandCancellation,
}

#[async_trait]
impl ToolExecutor for V2Executor<'_> {
    async fn execute(&self, call: &ToolCall) -> ToolResultMessage {
        if self.cancellation.is_cancelled() {
            return ToolResultMessage::for_call(call, "cancelled".to_string());
        }
        if call.name == crate::ai::doener_tool::DOENER_TOOL_NAME {
            return crate::ai::doener_tool::execute_doener_index(self.doener, call).await;
        }
        if call.name == crate::ai::tarot_tool::TAROT_TOOL_NAME {
            return crate::ai::tarot_tool::execute_draw_tarot(call).await;
        }
        if call.name == crate::ai::flight_tool::PLAN_FLIGHT_TOOL_NAME {
            return crate::ai::flight_tool::execute_plan_flight(call).await;
        }
        if call.name == crate::ai::packages_tool::SEARCH_PACKAGES_TOOL_NAME {
            return crate::ai::packages_tool::execute_search_packages(self.repology, call).await;
        }
        if call.name == crate::ai::dhl_tool::TRACK_SHIPMENT_TOOL_NAME {
            return match self.dhl {
                Some(c) => crate::ai::dhl_tool::execute_track_shipment(c, call).await,
                None => ToolResultMessage::for_call(call, "unknown_tool".to_string()),
            };
        }
        if call.name == crate::ai::emote_stats_tool::EMOTE_STATS_TOOL_NAME {
            return match self.emote_tracker {
                Some(t) => crate::ai::emote_stats_tool::execute_emote_stats(t.clone(), call).await,
                None => ToolResultMessage::for_call(call, "unknown_tool".to_string()),
            };
        }
        if call.name == crate::ai::emote_tool::SEARCH_EMOTES_TOOL_NAME {
            return match self.emotes {
                Some(p) => crate::ai::emote_tool::execute_search_emotes(p, call).await,
                None => ToolResultMessage::for_call(call, "unknown_tool".to_string()),
            };
        }
        if content::is_web_tool(&call.name) {
            match self.web {
                Some(w) => w.execute_tool_call(call, self.trace).await,
                None => ToolResultMessage::for_call(call, "unknown_tool".to_string()),
            }
        } else {
            self.chat.execute(call).await
        }
    }
}

async fn forced_web_search_round(web: &AiWeb, query: &str, trace: &TraceIds) -> ToolCallRound {
    let call = ToolCall {
        id: "forced_web_search_1".to_string(),
        name: "web_search".to_string(),
        arguments: serde_json::json!({
            "query": query,
            "max_results": web.executor.max_results(),
        }),
        arguments_parse_error: None,
    };
    let result = web.executor.execute_tool_call(&call, trace).await;
    ToolCallRound {
        calls: vec![call],
        results: vec![result],
        reasoning_content: None,
    }
}

/// Build the user-message instruction block when the viewer's `!ai` / `@grok`
/// turn includes a Twitch reply parent.
///
/// Utility parents stay on the viewer path (`parent_agent == None` after
/// filtering `Agent::Utility`). Mouth parents use the agent's settings
/// persona in the author label, not the Mouth login.
fn recap_profile_is_set(recap_profile: Option<&str>) -> bool {
    recap_profile.is_some_and(|m| !m.trim().is_empty())
}

fn recap_enabled_for_profile(recap_profile: Option<&str>, parked: bool) -> bool {
    recap_profile_is_set(recap_profile) && !parked
}

async fn union_recap_sheet_logins(
    handle: &RecapHandle,
    invocation: RecapChannel,
) -> BTreeSet<String> {
    let (first, second) = match invocation {
        RecapChannel::Primary => (RecapChannel::Primary, RecapChannel::AiChannel),
        RecapChannel::AiChannel => (RecapChannel::AiChannel, RecapChannel::Primary),
    };
    let mut out = BTreeSet::new();
    for login in handle.sheet_logins(first).await {
        out.insert(login);
        if out.len() >= RECAP_SHEET_LOGINS {
            return out;
        }
    }
    for login in handle.sheet_logins(second).await {
        out.insert(login);
        if out.len() >= RECAP_SHEET_LOGINS {
            break;
        }
    }
    out
}

/// Stable system-message appendix naming both agents' Twitch accounts.
///
/// Logins come from [`crate::twitch::mouth::MouthRouter::login_for`]; when
/// `None`, the copy states that replies currently go through the utility bot
/// login without denying the agent has an account. `Agent::Utility` in the
/// match arm folds into Aurora's German copy only for exhaustiveness.
/// Map a provider-side LLM failure to a short German chat reply.
///
/// 5xx and decode/transport errors stay silent — they are already logged at
/// error! and are usually transient. Authentication, payment, and rate-limit
/// problems get a hint in chat so the bot doesn't appear to silently swallow
/// `!ai` requests when, e.g., the OpenRouter wallet is empty.
fn user_facing_provider_message(err: &LlmError) -> Option<&'static str> {
    match err {
        LlmError::Provider { status, .. } => match *status {
            402 => Some("KI-Konto leer FeelsBadMan"),
            429 => Some("KI gerade rate-limited Stare"),
            401 | 403 => Some("KI-Konfiguration kaputt FeelsBadMan"),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod recap_wiring_tests {
    use super::{recap_enabled_for_profile, recap_profile_is_set};

    #[test]
    fn recap_enabled_follows_profile_and_parked_gate() {
        assert!(!recap_enabled_for_profile(None, false));
        assert!(!recap_enabled_for_profile(Some(""), false));
        assert!(!recap_enabled_for_profile(Some("   "), false));
        assert!(recap_enabled_for_profile(Some("tiny"), false));
        assert!(!recap_enabled_for_profile(Some("tiny"), true));
        assert!(recap_profile_is_set(Some("tiny")));
        assert!(!recap_profile_is_set(Some("  ")));
    }

    /// Parking stops new notes from being written; it must not hide the notes
    /// already on file. The preset injects whatever `render_suffix` returns, so
    /// that is where the behaviour lives.
    #[tokio::test]
    async fn parked_recap_still_renders_existing_notes() {
        use crate::ai::recap::{RecapChannel, RecapHandle, RecapNote};

        let dir = tempfile::tempdir().expect("tempdir");
        let handle = RecapHandle::load(dir.path().to_path_buf()).await;
        {
            let mut state = handle.inner.lock().await;
            state.parked = true;
            state.file.primary.notes.push(RecapNote {
                body: "existing recap note".into(),
                identities: Vec::new(),
                through_seq: 1,
                generation: 0,
            });
        }

        assert!(!recap_enabled_for_profile(Some("tiny"), true));
        assert!(recap_profile_is_set(Some("tiny")));
        let suffix = handle
            .render_suffix(RecapChannel::Primary, "main", "ai", 3)
            .await;
        assert!(suffix.contains("## Recap"));
        assert!(suffix.contains("existing recap note"));
    }
}

#[cfg(test)]
mod turn_tests {
    //! Prompt-assembly tests that drive [`TurnRunner::run_turn`] directly.
    //! No IRC, no `TestBotBuilder` — a captured `ToolChatCompletionRequest`
    //! is the whole assertion surface.
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use llm::{
        ChatCompletionRequest, LlmClient, LlmError, ToolChatCompletionRequest,
        ToolChatCompletionResponse,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::ai::command::AiMemoryV2;
    use crate::ai::memory::store::MemoryStore;
    use crate::ai::memory::transcript::TranscriptWriter;
    use crate::settings::Settings;
    use crate::settings::provider::AiProvider;
    use crate::twitch::echo::MouthIdentities;
    use crate::twitch::mouth::MouthRouter;

    /// Records every tool request and answers with a canned outcome.
    struct CapturingLlm {
        calls: Mutex<Vec<ToolChatCompletionRequest>>,
        answer: Answer,
    }

    #[derive(Clone)]
    enum Answer {
        Text(String),
        /// Sleep past the per-round timeout so `run_agent` gives up.
        Hang,
        Provider(u16),
    }

    #[async_trait]
    impl LlmClient for CapturingLlm {
        async fn chat_completion(&self, _r: ChatCompletionRequest) -> llm::Result<String> {
            // Only reached by the reword pass, which these tests stay under.
            Ok("reworded".into())
        }

        async fn chat_completion_with_tools(
            &self,
            request: ToolChatCompletionRequest,
        ) -> llm::Result<ToolChatCompletionResponse> {
            self.calls.lock().unwrap().push(request);
            match &self.answer {
                Answer::Text(t) => Ok(ToolChatCompletionResponse::Message(t.clone())),
                Answer::Hang => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(ToolChatCompletionResponse::Message(String::new()))
                }
                Answer::Provider(status) => Err(LlmError::Provider {
                    status: *status,
                    body: "nope".into(),
                }),
            }
        }
    }

    struct Fixture {
        runner: TurnRunner,
        llm: Arc<CapturingLlm>,
        _dir: TempDir,
    }

    async fn fixture(answer: Answer, timeout_secs: u64) -> Fixture {
        crate::install_crypto_provider();
        let dir = TempDir::new().unwrap();
        let mut s = Settings::compiled_defaults();
        // OpenRouter would make `ModelCatalog::display_name` reach the
        // network for the `{model}` substitution; any other provider
        // short-circuits to the raw model id.
        s.ai.profiles[0].provider = AiProvider::OpenAiCompatible;
        s.ai.profiles[0].model = "test-model".into();
        s.ai.profiles[0].timeout = timeout_secs;
        let settings: SettingsHandle = SettingsHandle::from_settings(s);

        let store = MemoryStore::open(dir.path(), settings.clone())
            .await
            .unwrap();
        let transcript = TranscriptWriter::open(store.memories_dir()).await.unwrap();
        let llm = Arc::new(CapturingLlm {
            calls: Mutex::new(Vec::new()),
            answer,
        });

        let runner = TurnRunner::new(AiCommandDeps {
            registry: Arc::new(crate::llm_factory::LlmRegistry::fixed(
                llm.clone() as Arc<dyn LlmClient>,
                settings.clone(),
            )),
            settings: settings.clone(),
            chat_ctx: None,
            memory: AiMemoryV2 {
                store,
                transcript,
                inject_byte_budget: 8192,
                max_turn_rounds: 3,
                max_writes_per_turn: 2,
                min_response_delay: Duration::ZERO,
            },
            web: None,
            emotes: None,
            bot_username: "utilitybot".into(),
            doener: Arc::new(crate::doener::DoeneratlasClient::with_base_url(
                reqwest::Client::new(),
                "http://127.0.0.1:1".to_string(),
            )),
            repology: Arc::new(crate::repology::RepologyClient::new().unwrap()),
            pronouns: Arc::new(crate::pronouns::PronounsClient::new().unwrap()),
            model_catalog: Arc::new(ModelCatalog::new(reqwest::Client::new())),
            mouths: Arc::new(MouthRouter {
                aurora: None,
                grok: None,
            }),
            echo: Arc::new(crate::twitch::echo::EchoGuard::new(
                MouthIdentities::default(),
            )),
            dhl: None,
            emote_tracker: None,
            recap: None,
            langfuse: None,
        });

        Fixture {
            runner,
            llm,
            _dir: dir,
        }
    }

    fn input(agent: Agent, instruction: &str) -> TurnInput {
        TurnInput {
            agent,
            instruction: instruction.into(),
            speaker: TurnSpeaker {
                login: "alice".into(),
                display_name: "Alice".into(),
                user_id: "111".into(),
                role: Role::Regular,
            },
            channel_login: "testchan".into(),
            channel_id: "222".into(),
            reply_parent: None,
            cancellation: CommandCancellation::new(),
        }
    }

    fn part(req: &ToolChatCompletionRequest, role: llm::Role) -> String {
        req.messages
            .iter()
            .find(|m| m.role == role)
            .expect("message for role")
            .content
            .clone()
    }

    /// Memory belongs in the cached system prefix; scratch, recent chat and
    /// the instruction belong in the volatile user message. Getting this
    /// backwards busts the prompt cache on every turn.
    #[tokio::test]
    async fn memory_lands_in_system_and_instruction_in_user() {
        let f = fixture(Answer::Text("moin".into()), 30).await;
        let out = f
            .runner
            .run_turn(input(Agent::Aurora, "sag hallo"))
            .await
            .unwrap();
        assert!(matches!(out, TurnOutcome::Reply { .. }));

        let calls = f.llm.calls.lock().unwrap();
        let system = part(&calls[0], llm::Role::System);
        let user = part(&calls[0], llm::Role::User);

        assert!(
            system.contains("kind=soul"),
            "memory bodies belong in system"
        );
        assert!(
            !user.contains("kind=soul"),
            "memory must not leak into user"
        );
        assert!(user.contains("sag hallo"), "instruction belongs in user");
        assert!(
            !system.contains("sag hallo"),
            "instruction must not enter the cached prefix"
        );
    }

    /// The per-process nonce is stable, so two turns whose memory has not
    /// changed must produce a byte-identical system message — that identity
    /// is the entire prompt-cache contract.
    #[tokio::test]
    async fn system_prefix_is_byte_stable_across_turns() {
        let f = fixture(Answer::Text("moin".into()), 30).await;
        f.runner
            .run_turn(input(Agent::Aurora, "erste frage"))
            .await
            .unwrap();
        f.runner
            .run_turn(input(Agent::Aurora, "zweite frage"))
            .await
            .unwrap();

        let calls = f.llm.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            part(&calls[0], llm::Role::System),
            part(&calls[1], llm::Role::System),
            "system prefix drifted between turns"
        );
        assert_ne!(
            part(&calls[0], llm::Role::User),
            part(&calls[1], llm::Role::User),
        );
    }

    /// Two agents, two memory postures: Aurora gets bodies injected, grok
    /// gets a paths-only manifest plus `read_memory`, and no write tools.
    #[tokio::test]
    async fn grok_gets_a_manifest_and_no_write_tools() {
        let aurora = fixture(Answer::Text("moin".into()), 30).await;
        aurora
            .runner
            .run_turn(input(Agent::Aurora, "hi"))
            .await
            .unwrap();
        let grok = fixture(Answer::Text("yep".into()), 30).await;
        grok.runner
            .run_turn(input(Agent::Grok, "hi"))
            .await
            .unwrap();

        let a = aurora.llm.calls.lock().unwrap();
        let g = grok.llm.calls.lock().unwrap();

        assert!(part(&a[0], llm::Role::System).contains("kind=soul"));
        assert!(
            !part(&g[0], llm::Role::System).contains("kind=soul"),
            "grok must not receive memory bodies"
        );

        let names = |r: &ToolChatCompletionRequest| {
            r.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
        };
        assert!(names(&a[0]).contains(&"write_file".to_string()));
        assert!(!names(&g[0]).contains(&"write_file".to_string()));
        assert!(names(&g[0]).contains(&"read_memory".to_string()));
    }

    /// A per-round timeout and a provider failure both owe the viewer a
    /// short notice; a blank answer owes them nothing.
    #[tokio::test(start_paused = true)]
    async fn timeout_maps_to_a_notice() {
        let f = fixture(Answer::Hang, 1).await;
        let out = f.runner.run_turn(input(Agent::Aurora, "hi")).await.unwrap();
        assert_eq!(out, TurnOutcome::Notice(TIMEOUT_NOTICE));
    }

    #[tokio::test]
    async fn provider_failure_and_blank_answer_map_to_notice_and_silence() {
        let paid = fixture(Answer::Provider(402), 30).await;
        assert_eq!(
            paid.runner
                .run_turn(input(Agent::Aurora, "hi"))
                .await
                .unwrap(),
            TurnOutcome::Notice("KI-Konto leer FeelsBadMan"),
        );

        // 500 stays silent: transient, already logged at error!.
        let boom = fixture(Answer::Provider(500), 30).await;
        assert_eq!(
            boom.runner
                .run_turn(input(Agent::Aurora, "hi"))
                .await
                .unwrap(),
            TurnOutcome::Silent,
        );

        let blank = fixture(Answer::Text("   ".into()), 30).await;
        assert_eq!(
            blank
                .runner
                .run_turn(input(Agent::Aurora, "hi"))
                .await
                .unwrap(),
            TurnOutcome::Silent,
        );
    }
}
