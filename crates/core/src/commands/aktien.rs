//! `!aktien` (alias `!stocks`): joke "stocks to buy now". Quotes a baked
//! ticker pool on Finnhub and names the day's two best performers plus the
//! worst one ("buy the dip"). Finnhub's free tier has no top-movers
//! endpoint, so the pool is the whole market as far as this command knows.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use eyre::Result;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use twitch_irc::{login::LoginCredentials, transport::Transport};

use super::{
    Command, CommandContext, ExecutionClass, aktien_cooldown_duration,
    reply_emotes::{ReplyEmoteCategory, cooldown_reply, error_reply},
};
use crate::{
    cooldown::PerUserCooldown,
    finnhub::{FinnhubClient, FinnhubError},
    settings::SettingsHandle,
};

/// One pool entry: the Finnhub symbol and the name chat sees.
#[derive(Debug, Clone, Copy)]
struct Ticker {
    symbol: &'static str,
    label: &'static str,
}

const fn t(symbol: &'static str, label: &'static str) -> Ticker {
    Ticker { symbol, label }
}

/// The candidate pool. The free tier quotes US listings only, so DAX names
/// go through their US listing or ADR and keep a German label. One
/// invocation costs one request per entry against the 60 calls/min limit.
const POOL: &[Ticker] = &[
    // DAX
    t("SAP", "SAP"),
    t("SIEGY", "Siemens"),
    t("ALIZY", "Allianz"),
    t("DTEGY", "Telekom"),
    t("MBGYY", "Mercedes"),
    t("BMWYY", "BMW"),
    t("VWAGY", "VW"),
    t("BAYRY", "Bayer"),
    t("BASFY", "BASF"),
    t("ADDYY", "Adidas"),
    t("IFNNY", "Infineon"),
    t("RNMBY", "Rheinmetall"),
    t("DB", "Deutsche Bank"),
    // US large caps
    t("AAPL", "AAPL"),
    t("MSFT", "MSFT"),
    t("NVDA", "NVDA"),
    t("AMZN", "AMZN"),
    t("GOOGL", "GOOGL"),
    t("META", "META"),
    t("TSLA", "TSLA"),
    t("NFLX", "NFLX"),
    // Meme stocks
    t("GME", "GME"),
    t("AMC", "AMC"),
    t("BB", "BB"),
    t("KOSS", "KOSS"),
    t("HOOD", "HOOD"),
];

/// Quote requests in flight at once.
const MAX_CONCURRENT_QUOTES: usize = 6;
/// How long a computed pick is reused across viewers.
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// How long an error reply is reused, so callers queued behind a failed
/// round (e.g. a 429) do not each retry the whole pool upstream.
const ERROR_CACHE_TTL: Duration = Duration::from_secs(45);

const UPSTREAM_ERROR_REPLY: &str = "Die Börse geht gerade nicht ran, keine Tipps heute";
const TOO_FEW_QUOTES_REPLY: &str = "Zu wenige Kurse gerade, der Markt pennt wohl";

#[derive(Debug, thiserror::Error)]
enum PickError {
    #[error("quote for {symbol} failed")]
    Upstream {
        symbol: &'static str,
        #[source]
        source: FinnhubError,
    },
    #[error("only {usable} usable quotes")]
    TooFewQuotes { usable: usize },
}

/// The two best performers and the worst one, as (label, day change %).
#[derive(Debug, Clone, PartialEq)]
struct Picks {
    top: [(&'static str, f64); 2],
    dip: (&'static str, f64),
}

pub struct AktienCommand {
    finnhub: Arc<FinnhubClient>,
    cooldown: PerUserCooldown,
    pool: &'static [Ticker],
    cache_ttl: Duration,
    error_ttl: Duration,
    /// Last reply (pick or error line) and when it expires. The lock is held
    /// across the fetch, so viewers arriving mid-fetch wait for that round
    /// instead of starting their own.
    cache: tokio::sync::Mutex<Option<(Instant, String, bool)>>,
}

impl AktienCommand {
    pub fn new(finnhub: Arc<FinnhubClient>, settings: SettingsHandle) -> Self {
        Self {
            finnhub,
            cooldown: PerUserCooldown::live(settings, aktien_cooldown_duration),
            pool: POOL,
            cache_ttl: CACHE_TTL,
            error_ttl: ERROR_CACHE_TTL,
            cache: tokio::sync::Mutex::new(None),
        }
    }

    async fn categorized_reply(&self) -> (String, bool) {
        let mut cache = self.cache.lock().await;
        if let Some((expires, reply, is_error)) = cache.as_ref()
            && Instant::now() < *expires
        {
            return (reply.clone(), *is_error);
        }
        let (reply, ttl, is_error) = match self.pick().await {
            Ok(picks) => (picks_reply(&picks), self.cache_ttl, false),
            Err(error) => {
                tracing::warn!(?error, "aktien pick failed");
                (pick_error_reply(&error).to_owned(), self.error_ttl, true)
            }
        };
        *cache = Some((Instant::now() + ttl, reply.clone(), is_error));
        (reply, is_error)
    }

    #[cfg(test)]
    async fn reply(&self) -> String {
        self.categorized_reply().await.0
    }

    async fn pick(&self) -> Result<Picks, PickError> {
        let quoted: Vec<Option<(usize, &'static str, f64)>> =
            stream::iter(self.pool.iter().copied().enumerate())
                .map(|(i, ticker)| async move {
                    self.finnhub
                        .change_percent(ticker.symbol)
                        .await
                        .map(|dp| dp.map(|dp| (i, ticker.label, dp)))
                        .map_err(|source| PickError::Upstream {
                            symbol: ticker.symbol,
                            source,
                        })
                })
                .buffer_unordered(MAX_CONCURRENT_QUOTES)
                .try_collect()
                .await?;
        let mut quotes: Vec<_> = quoted.into_iter().flatten().collect();
        // Best first; pool order breaks ties so the pick is deterministic.
        quotes.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
        let [first, second, .., dip] = quotes.as_slice() else {
            return Err(PickError::TooFewQuotes {
                usable: quotes.len(),
            });
        };
        let labeled = |&(_, label, dp): &(usize, &'static str, f64)| (label, dp);
        Ok(Picks {
            top: [labeled(first), labeled(second)],
            dip: labeled(dip),
        })
    }
}

/// `+4,2%` / `-5,3%`: signed, one decimal, German decimal comma.
fn signed_percent(dp: f64) -> String {
    let rounded = format!("{dp:+.1}%");
    // Values in (-0.05, 0) round to negative zero.
    if rounded == "-0.0%" {
        return "+0,0%".to_owned();
    }
    rounded.replace('.', ",")
}

fn picks_reply(p: &Picks) -> String {
    let [(a, a_dp), (b, b_dp)] = p.top;
    let (dip, dip_dp) = p.dip;
    format!(
        "Aktien-Tipps vom Profi: {a} {}, {b} {} und {dip} {} (buy the dip) 📈",
        signed_percent(a_dp),
        signed_percent(b_dp),
        signed_percent(dip_dp),
    )
}

fn pick_error_reply(e: &PickError) -> &'static str {
    match e {
        PickError::Upstream { .. } => UPSTREAM_ERROR_REPLY,
        PickError::TooFewQuotes { .. } => TOO_FEW_QUOTES_REPLY,
    }
}

#[async_trait]
impl<T, L> Command<T, L> for AktienCommand
where
    T: Transport,
    L: LoginCredentials,
{
    fn id(&self) -> &'static str {
        "aktien"
    }

    fn name(&self) -> &str {
        "!aktien"
    }

    fn aliases(&self) -> &[&'static str] {
        &["!stocks"]
    }

    fn description(&self) -> &str {
        "Drei heiße Aktien-Tipps: die zwei Tagesgewinner und der größte Verlierer"
    }

    fn usage(&self) -> &str {
        "!aktien"
    }

    fn execution_policy(&self) -> ExecutionClass {
        ExecutionClass::Background
    }

    async fn execute(&self, ctx: CommandContext<T, L>) -> Result<()> {
        ctx.cancellation.check()?;
        if let Err(remaining) = self.cooldown.try_acquire(&ctx.privmsg.sender.login).await {
            ctx.sender
                .reply(&ctx.privmsg, cooldown_reply(&ctx.settings, remaining))
                .await;
            return Ok(());
        }
        let (reply, is_error) = self.categorized_reply().await;
        ctx.cancellation.check()?;
        let reply = if is_error {
            error_reply(&ctx.settings, ReplyEmoteCategory::Failure, reply)
        } else {
            reply
        };
        ctx.sender.reply(&ctx.privmsg, reply).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    use super::*;

    const SMALL_POOL: &[Ticker] = &[
        t("NVDA", "NVDA"),
        t("GME", "GME"),
        t("SAP", "SAP"),
        t("AAPL", "AAPL"),
        t("SIEGY", "Siemens"),
    ];

    fn quote(c: f64, dp: Option<f64>) -> ResponseTemplate {
        let dp = dp.map_or("null".to_owned(), |dp| dp.to_string());
        ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"c":{c},"d":null,"dp":{dp},"h":0,"l":0,"o":0,"pc":0,"t":0}}"#
        ))
    }

    async fn mount(server: &MockServer, symbol: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/quote"))
            .and(query_param("symbol", symbol))
            .respond_with(response)
            .expect(1)
            .mount(server)
            .await;
    }

    fn command(server: &MockServer, pool: &'static [Ticker]) -> AktienCommand {
        crate::install_crypto_provider();
        let finnhub = FinnhubClient::with_base_url(
            reqwest::Client::new(),
            server.uri(),
            SecretString::new("key".into()),
        );
        let mut cmd = AktienCommand::new(
            Arc::new(finnhub),
            SettingsHandle::from_settings(crate::settings::Settings::compiled_defaults()),
        );
        cmd.pool = pool;
        cmd
    }

    #[test]
    fn signed_percent_uses_sign_and_decimal_comma() {
        assert_eq!(signed_percent(4.19), "+4,2%");
        assert_eq!(signed_percent(-5.25), "-5,2%");
        assert_eq!(signed_percent(0.0), "+0,0%");
        assert_eq!(signed_percent(-0.04), "+0,0%");
        assert_eq!(signed_percent(-0.0), "+0,0%");
    }

    #[test]
    fn pool_has_no_duplicate_symbols() {
        let mut symbols: Vec<_> = POOL.iter().map(|t| t.symbol).collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), POOL.len(), "duplicate symbol in POOL");
    }

    #[tokio::test]
    async fn picks_two_best_and_the_worst() {
        let server = MockServer::start().await;
        mount(&server, "NVDA", quote(181.5, Some(4.19))).await;
        mount(&server, "GME", quote(22.1, Some(3.08))).await;
        mount(&server, "SAP", quote(240.0, Some(-5.3))).await;
        mount(&server, "AAPL", quote(270.0, Some(0.4))).await;
        mount(&server, "SIEGY", quote(130.0, Some(-1.2))).await;
        let cmd = command(&server, SMALL_POOL);
        assert_eq!(
            cmd.reply().await,
            "Aktien-Tipps vom Profi: NVDA +4,2%, GME +3,1% und SAP -5,3% (buy the dip) 📈"
        );
    }

    #[tokio::test]
    async fn fewer_than_three_usable_quotes_is_the_error_reply() {
        let server = MockServer::start().await;
        // Each excluded one would top or bottom the ranking if it counted.
        mount(&server, "NVDA", quote(0.0, Some(50.0))).await;
        mount(&server, "GME", quote(22.1, None)).await;
        mount(
            &server,
            "SAP",
            ResponseTemplate::new(403).set_body_string(r#"{"error":"no access"}"#),
        )
        .await;
        mount(&server, "AAPL", quote(270.0, Some(0.4))).await;
        mount(&server, "SIEGY", quote(130.0, Some(-1.2))).await;
        let cmd = command(&server, SMALL_POOL);
        assert_eq!(cmd.reply().await, TOO_FEW_QUOTES_REPLY);
    }

    #[tokio::test]
    async fn excluded_quotes_leave_the_rest_ranked() {
        static POOL4: &[Ticker] = &[
            t("NVDA", "NVDA"),
            t("GME", "GME"),
            t("AAPL", "AAPL"),
            t("SIEGY", "Siemens"),
        ];
        let server = MockServer::start().await;
        mount(
            &server,
            "NVDA",
            ResponseTemplate::new(200)
                .set_body_string(r#"{"c":0,"d":null,"dp":null,"h":0,"l":0,"o":0,"pc":0,"t":0}"#),
        )
        .await;
        mount(&server, "GME", quote(22.1, Some(3.08))).await;
        mount(&server, "AAPL", quote(270.0, Some(0.4))).await;
        mount(&server, "SIEGY", quote(130.0, Some(-1.2))).await;
        let cmd = command(&server, POOL4);
        assert_eq!(
            cmd.reply().await,
            "Aktien-Tipps vom Profi: GME +3,1%, AAPL +0,4% und Siemens -1,2% (buy the dip) 📈"
        );
    }

    #[tokio::test]
    async fn upstream_error_gives_the_error_reply_and_is_cached_briefly() {
        let server = MockServer::start().await;
        Mock::given(path("/quote"))
            .and(query_param("symbol", "SAP"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        Mock::given(path("/quote"))
            .respond_with(quote(10.0, Some(1.0)))
            .mount(&server)
            .await;
        let cmd = command(&server, SMALL_POOL);
        assert_eq!(cmd.reply().await, UPSTREAM_ERROR_REPLY);
        // A caller queued behind the failed round gets the cached error
        // without retrying upstream.
        assert_eq!(cmd.reply().await, UPSTREAM_ERROR_REPLY);
        // The pool fits in one concurrency window, so the failed round put
        // every request in flight at once; quotes still in flight when the
        // 429 aborted the round may land after it returns. Any retry would
        // push the count past one request per symbol.
        assert!(SMALL_POOL.len() <= MAX_CONCURRENT_QUOTES);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let received = server.received_requests().await.map_or(0, |r| r.len());
        assert!(
            received <= SMALL_POOL.len(),
            "{received} requests for {} symbols: the error was not cached",
            SMALL_POOL.len()
        );
    }

    #[tokio::test]
    async fn expired_error_cache_fetches_again() {
        let server = MockServer::start().await;
        Mock::given(path("/quote"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let mut cmd = command(&server, SMALL_POOL);
        cmd.error_ttl = Duration::ZERO;
        cmd.reply().await;
        let after_first = server.received_requests().await.map_or(0, |r| r.len());
        cmd.reply().await;
        let after_second = server.received_requests().await.map_or(0, |r| r.len());
        assert!(
            after_second > after_first,
            "{after_first} -> {after_second}"
        );
    }

    #[tokio::test]
    async fn second_call_within_ttl_makes_no_requests() {
        let server = MockServer::start().await;
        for (symbol, dp) in [
            ("NVDA", 4.19),
            ("GME", 3.08),
            ("SAP", -5.3),
            ("AAPL", 0.4),
            ("SIEGY", -1.2),
        ] {
            // `expect(1)` per symbol, verified when the server drops.
            mount(&server, symbol, quote(100.0, Some(dp))).await;
        }
        let cmd = command(&server, SMALL_POOL);
        let first = cmd.reply().await;
        let second = cmd.reply().await;
        assert_eq!(first, second);
        assert_eq!(server.received_requests().await.map(|r| r.len()), Some(5));
    }

    #[tokio::test]
    async fn expired_cache_fetches_again() {
        let server = MockServer::start().await;
        Mock::given(path("/quote"))
            .respond_with(quote(100.0, Some(1.0)))
            .expect(10)
            .mount(&server)
            .await;
        let mut cmd = command(&server, SMALL_POOL);
        cmd.cache_ttl = Duration::ZERO;
        cmd.reply().await;
        cmd.reply().await;
    }
}
