//! Signing in to a provider with a person's own subscription plan instead of
//! an API key: ChatGPT's Codex, Claude Pro/Max, Grok (X Premium), GitHub
//! Copilot and the rest of the plans RAST supports, the way 9router
//! (<https://github.com/decolua/9router>) and OpenCode do it: each provider's
//! own public OAuth client, with PKCE.
//!
//! Most providers send the browser back to an address that only exists on the
//! person's own machine (`http://localhost:…`), which a server elsewhere never
//! receives: the person copies that page's address and pastes it into RAST,
//! which checks the `state` and trades the `code` for tokens (a browser flow).
//! GitHub Copilot, Qwen and Kimi instead show a short code to type on their
//! site and RAST polls for the tokens (a device flow). The Claude Code CLI's
//! login can also be imported when the server already runs it.
//!
//! Access tokens last about an hour; RAST refreshes them ahead of time and
//! one refresh at a time, keeping the new refresh token at once (most
//! providers rotate refresh tokens and end the whole session when an old one
//! is used again). Tokens are stored encrypted and never logged or shown.
//!
//! There is one file per provider (`codex.rs` is the reference
//! implementation, the rest follow it), so a provider's details stay in one
//! place. The shared pieces are `pkce.rs` (the verifier and challenge),
//! `http.rs` (the blocking requests) and `store.rs` (the sign-ins in flight,
//! kept in memory under their random `state` until they finish).

mod antigravity;
mod claude;
mod claude_import;
mod codex;
mod copilot;
mod gemini_cli;
pub(crate) mod google;
mod grok;
mod http;
mod iflow;
mod kimi;
mod pkce;
mod qwen;
mod store;

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::ai::Kind;

/// The Codex CLI version Codex's backend is told (newer models need a
/// recent one), as 9router sends it. Kept here because the provider's wire
/// code uses the same identity as the sign-in.
pub use codex::CODEX_VERSION;

/// Refresh when the access token has less than this left…
const REFRESH_AHEAD_MS: i64 = 10 * 60 * 1000;
/// …or when the last refresh is this old (refresh tokens go stale).
const REFRESH_EVERY_MS: i64 = 8 * 24 * 60 * 60 * 1000;

/// How a plan signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flow {
    /// A page in the browser; the person pastes the address it ends on.
    Browser,
    /// A code to type on the provider's site; RAST polls for the tokens.
    Device,
}

/// A subscription plan RAST can sign in to: one per provider kind in the
/// "plan" group. Each names the file in this module that implements it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Plan {
    Codex,
    Claude,
    Grok,
    Copilot,
    Antigravity,
    GeminiCli,
    Qwen,
    IFlow,
    Kimi,
}

/// Every plan's risk note opens with the same ownership fact: a signed-in
/// plan is its signer's alone and spends that person's quota. Each match arm
/// below adds what its provider allows.
macro_rules! risk_note {
    ($specific:literal) => {
        concat!(
            "A plan is one person's: only you can chat with it here, and replies count toward its limits. ",
            $specific
        )
    };
}

impl Plan {
    /// Every plan, in the order the settings page offers them.
    pub const ALL: [Plan; 9] = [
        Plan::Codex,
        Plan::Claude,
        Plan::Grok,
        Plan::Copilot,
        Plan::Antigravity,
        Plan::GeminiCli,
        Plan::Qwen,
        Plan::IFlow,
        Plan::Kimi,
    ];

    /// The kind id in the database and the API (`codex`, `gemini-cli`, …).
    pub fn id(self) -> &'static str {
        self.kind().as_str()
    }

    /// What the sign-in dialog calls it.
    pub fn label(self) -> &'static str {
        match self {
            Plan::Codex => "ChatGPT",
            Plan::Claude => "Claude",
            Plan::Grok => "Grok (X Premium)",
            Plan::Copilot => "GitHub Copilot",
            Plan::Antigravity => "Google Antigravity",
            Plan::GeminiCli => "Gemini CLI",
            Plan::Qwen => "Qwen",
            Plan::IFlow => "iFlow",
            Plan::Kimi => "Kimi",
        }
    }

    /// The provider kind this plan signs in to.
    pub fn kind(self) -> Kind {
        match self {
            Plan::Codex => Kind::Codex,
            Plan::Claude => Kind::Claude,
            Plan::Grok => Kind::Grok,
            Plan::Copilot => Kind::Copilot,
            Plan::Antigravity => Kind::Antigravity,
            Plan::GeminiCli => Kind::GeminiCli,
            Plan::Qwen => Kind::QwenPlan,
            Plan::IFlow => Kind::IFlow,
            Plan::Kimi => Kind::KimiPlan,
        }
    }

    /// The plan behind a kind (none for API-key and API kinds, including the
    /// `qwen` and `moonshot` kinds that have plan counterparts).
    pub fn of_kind(kind: Kind) -> Option<Plan> {
        match kind {
            Kind::Codex => Some(Plan::Codex),
            Kind::Claude => Some(Plan::Claude),
            Kind::Grok => Some(Plan::Grok),
            Kind::Copilot => Some(Plan::Copilot),
            Kind::Antigravity => Some(Plan::Antigravity),
            Kind::GeminiCli => Some(Plan::GeminiCli),
            Kind::QwenPlan => Some(Plan::Qwen),
            Kind::IFlow => Some(Plan::IFlow),
            Kind::KimiPlan => Some(Plan::Kimi),
            _ => None,
        }
    }

    /// How this provider signs in.
    pub fn flow(self) -> Flow {
        match self {
            Plan::Copilot | Plan::Qwen | Plan::Kimi => Flow::Device,
            _ => Flow::Browser,
        }
    }

    /// What the dialog's sign-in button offers, after "Continue with"
    /// ("ChatGPT", "GitHub", …).
    pub fn site(self) -> &'static str {
        match self {
            Plan::Codex => "ChatGPT",
            Plan::Claude => "Claude",
            Plan::Grok => "xAI",
            Plan::Copilot => "GitHub",
            Plan::Antigravity | Plan::GeminiCli => "Google",
            Plan::Qwen => "Qwen",
            Plan::IFlow => "iFlow",
            Plan::Kimi => "Kimi",
        }
    }

    /// The placeholder for the paste field in the sign-in dialog ("" when the
    /// plan polls a device code instead).
    pub fn callback_hint(self) -> &'static str {
        match self {
            Plan::Codex => "http://localhost:1455/auth/callback?code=…",
            Plan::Claude => {
                "http://localhost:53692/callback?code=… (or the code#state Claude shows)"
            }
            Plan::Grok => "http://127.0.0.1:56121/callback?code=…",
            Plan::Antigravity => "http://localhost:56123/oauth-callback?code=…",
            Plan::GeminiCli => "http://localhost:8085/oauth2callback?code=…",
            Plan::IFlow => "the address iFlow sends you back to (with code= and state=)",
            Plan::Copilot | Plan::Qwen | Plan::Kimi => "",
        }
    }

    /// One line the dialog shows before signing in ("" none). Every plan
    /// starts with the same ownership fact, then says what its provider
    /// allows.
    pub fn risk_note(self) -> &'static str {
        match self {
            Plan::Codex => risk_note!(
                "OpenAI only lets the Codex CLI sign in this way; if they tighten it, the sign-in may stop working."
            ),
            Plan::Claude => risk_note!(
                "Anthropic only allows a personal plan through Claude Code; RAST presents itself as Claude Code."
            ),
            Plan::Grok => risk_note!(
                "xAI only allows X Premium access through its own CLI; the sign-in presents as the Grok CLI."
            ),
            Plan::Copilot => risk_note!(
                "GitHub only allows plan access from an editor's Copilot client; RAST presents as VS Code's Copilot Chat."
            ),
            Plan::Antigravity => risk_note!(
                "Google's Antigravity endpoints are private; using them from outside the IDE risks account restriction."
            ),
            Plan::GeminiCli => risk_note!(
                "Google's Code Assist endpoints are for the Gemini CLI; using them outside it risks account restriction."
            ),
            Plan::Qwen => risk_note!("Qwen's plan tokens are meant for its own clients."),
            Plan::IFlow => risk_note!("iFlow's plan tokens are meant for its own clients."),
            Plan::Kimi => {
                risk_note!("Kimi Code tokens are meant for the Kimi CLI; the sign-in presents as it.")
            }
        }
    }
}

/// A signed-in plan's tokens and who it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Milliseconds since the epoch.
    pub expires_at: i64,
    pub refreshed_at: i64,
    /// The provider's account id, sent with every request where one is
    /// needed (ChatGPT's workspace, Google's project, GitHub's login).
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub email: String,
    /// "plus", "pro", "max", … (what the provider says the plan is).
    #[serde(default)]
    pub plan: String,
    /// The Google Cloud project Cloud Code requests go to ("" otherwise).
    #[serde(default)]
    pub project_id: String,
}

impl Credentials {
    pub fn needs_refresh(&self, now_ms: i64) -> bool {
        self.expires_at - now_ms < REFRESH_AHEAD_MS || now_ms - self.refreshed_at > REFRESH_EVERY_MS
    }

    pub fn expired(&self, now_ms: i64) -> bool {
        self.expires_at <= now_ms
    }
}

/// A JWT's claims, unverified: the tokens come straight from the provider
/// over TLS, and RAST only reads who signed in from them.
pub(crate) fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&pkce::decode_url_safe(payload)?).ok()
}

/// A sign-in started and waiting for the person to answer it.
#[derive(Debug, Clone)]
pub enum Started {
    /// Open `url` in the browser, then paste the address it ends on.
    Browser {
        url: String,
        /// Names this sign-in (also in the address).
        state: String,
        redirect_uri: String,
    },
    /// Enter `user_code` on `verification_uri`; RAST polls every `interval`
    /// seconds for up to `expires_in` seconds.
    Device {
        state: String,
        user_code: String,
        verification_uri: String,
        interval: u64,
        expires_in: u64,
    },
}

/// A device sign-in that finished: whose it is and the tokens it won.
#[derive(Debug, Clone)]
pub struct SignedIn {
    pub plan: Plan,
    pub name: String,
    /// Signing in again to this provider (else a new one).
    pub provider_id: Option<String>,
    pub credentials: Credentials,
}

/// The result of one device-flow poll.
#[derive(Debug, Clone)]
pub enum Poll {
    /// The person has not finished on the provider's page yet.
    Waiting,
    /// Signed in; the pending entry was consumed.
    Done(Box<SignedIn>),
    /// The provider says it will not work (expired code, access denied, …).
    Failed(String),
}

/// What a pasted address carries.
#[derive(Debug, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub state: String,
}

/// A sign-in started and waiting for its answer (a browser paste or a device
/// poll). Callers see who it is for and its plan; the PKCE verifier and the
/// device code stay inside this module.
#[derive(Debug, Clone)]
pub struct Pending {
    pub plan: Plan,
    pub user_id: i64,
    /// Signing in again to this provider (else a new one).
    pub provider_id: Option<String>,
    pub name: String,
    /// The OAuth state this sign-in was registered under: the token
    /// exchange must echo it for providers that check it (Claude does).
    pub(crate) state: String,
    /// The PKCE verifier the token exchange must present (browser flows).
    pub(crate) verifier: String,
    /// Where the browser was told to return (the exchange repeats it).
    pub(crate) redirect_uri: String,
    /// The code polled for a token (device flows).
    pub(crate) device_code: String,
    /// Seconds between poll attempts (device flows).
    pub(crate) interval: u64,
    /// The earliest instant the next poll is allowed (device flows).
    pub(crate) next_poll: Instant,
    /// How long the device code is good for (device flows).
    pub(crate) expires_in: u64,
    /// When it started; entries die after 20 minutes.
    pub(crate) started: Instant,
}

impl Pending {
    /// A browser flow's pending entry: the verifier and redirect the token
    /// exchange needs.
    pub(crate) fn browser(
        plan: Plan,
        user_id: i64,
        provider_id: Option<String>,
        name: String,
        state: String,
        verifier: String,
        redirect_uri: String,
    ) -> Pending {
        Pending {
            plan,
            user_id,
            provider_id,
            name,
            state,
            verifier,
            redirect_uri,
            device_code: String::new(),
            interval: 0,
            next_poll: Instant::now(),
            expires_in: 0,
            started: Instant::now(),
        }
    }

    /// A device flow's pending entry: the code to poll and how often.
    #[allow(clippy::too_many_arguments)] // every field of the pending is named here
    pub(crate) fn device(
        plan: Plan,
        user_id: i64,
        provider_id: Option<String>,
        name: String,
        state: String,
        device_code: String,
        interval: u64,
        expires_in: u64,
    ) -> Pending {
        Pending {
            plan,
            user_id,
            provider_id,
            name,
            state,
            verifier: String::new(),
            redirect_uri: String::new(),
            device_code,
            interval,
            next_poll: Instant::now(),
            expires_in,
            started: Instant::now(),
        }
    }
}

/// Start signing in: build the page to open (a browser flow) or the code to
/// enter (a device flow), and remember the sign-in under its random `state`
/// until it finishes. Nothing is stored in the database yet.
pub fn start(
    plan: Plan,
    user_id: i64,
    provider_id: Option<String>,
    name: String,
    now_ms: i64,
) -> Result<Started, String> {
    match plan {
        Plan::Codex => codex::start(user_id, provider_id, name, now_ms),
        Plan::Claude => claude::start(user_id, provider_id, name, now_ms),
        Plan::Grok => grok::start(user_id, provider_id, name, now_ms),
        Plan::Copilot => copilot::start(user_id, provider_id, name, now_ms),
        Plan::Antigravity => antigravity::start(user_id, provider_id, name, now_ms),
        Plan::GeminiCli => gemini_cli::start(user_id, provider_id, name, now_ms),
        Plan::Qwen => qwen::start(user_id, provider_id, name, now_ms),
        Plan::IFlow => iflow::start(user_id, provider_id, name, now_ms),
        Plan::Kimi => kimi::start(user_id, provider_id, name, now_ms),
    }
}

/// Read the address (or code) the browser ended on: the whole address, just
/// its `?code=…&state=…` query part, or Claude's copy-code form `code#state`.
///
/// The plan is only known once the `state` names a pending sign-in, so the
/// parse itself is the same for every provider (and the error copy is too);
/// what each provider's token endpoint said is worded by its own module.
pub fn parse_callback(text: &str) -> Result<Callback, String> {
    let text = text.trim();
    // Claude's copy-code page shows "<code>#<state>" rather than an address.
    if !text.contains('?') && !text.contains("code=") && text.contains('#') {
        let (code, state) = text.split_once('#').expect("the text contains #");
        let (code, state) = (code.trim(), state.trim());
        if !code.is_empty() && !state.is_empty() {
            return Ok(Callback {
                code: code.to_string(),
                state: state.to_string(),
            });
        }
        return Err(
            "that is not a sign-in code and state; copy the whole code (it has a #state at the end)"
                .into(),
        );
    }
    let query = match text.split_once('?') {
        Some((_, query)) => query,
        None if text.contains("code=") => text,
        None => {
            return Err(
                "paste the whole address of the page the sign-in sent you to (it starts with http://localhost:…/callback?code=) or its code#state"
                    .into(),
            )
        }
    };
    let query = query.split('#').next().unwrap_or("");
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        fields.insert(http::decode(key), http::decode(value));
    }
    if let Some(error) = fields.get("error") {
        let said = fields
            .get("error_description")
            .filter(|d| !d.is_empty())
            .unwrap_or(error);
        return Err(format!("the sign-in was refused: {said}"));
    }
    match (fields.remove("code"), fields.remove("state")) {
        (Some(code), Some(state)) if !code.is_empty() && !state.is_empty() => {
            Ok(Callback { code, state })
        }
        _ => Err(
            "that address has no sign-in code; paste the address of the page the sign-in sent you to after you allowed it"
                .into(),
        ),
    }
}

/// The sign-in a pasted address answers (once; only for who started it).
pub fn take_pending(state: &str, user_id: i64) -> Result<Pending, String> {
    store::take(state, user_id)
}

/// Browser flows: trade the pasted code for tokens (blocking network
/// allowed).
pub fn finish(pending: &Pending, code: &str, now_ms: i64) -> Result<Credentials, String> {
    match pending.plan {
        Plan::Codex => codex::exchange(pending, code, now_ms),
        Plan::Claude => claude::exchange(pending, code, now_ms),
        Plan::Grok => grok::exchange(pending, code, now_ms),
        Plan::Antigravity => antigravity::exchange(pending, code, now_ms),
        Plan::GeminiCli => gemini_cli::exchange(pending, code, now_ms),
        Plan::IFlow => iflow::exchange(pending, code, now_ms),
        Plan::Copilot | Plan::Qwen | Plan::Kimi => {
            Err("this sign-in uses a device code; it finishes by polling, not by pasting".into())
        }
    }
}

/// Device flows: one poll attempt (blocking network allowed). Enforces the
/// interval: returns [`Poll::Waiting`] when it is too early.
///
/// The pending entry is consumed only when the poll returns tokens, so two
/// concurrent polls cannot add the provider twice.
pub fn poll(state: &str, user_id: i64, now_ms: i64) -> Result<Poll, String> {
    let pending = store::get(state, user_id)?;
    if pending.plan.flow() != Flow::Device {
        return Err("this sign-in finishes with a pasted address, not a poll".into());
    }
    let now = Instant::now();
    if now < pending.next_poll {
        return Ok(Poll::Waiting);
    }
    // A code the person let expire cannot be approved any more: say so and
    // forget it rather than poll a dead code until the 20-minute sweep.
    if pending.expires_in > 0
        && pending.started.elapsed() >= std::time::Duration::from_secs(pending.expires_in)
    {
        store::take(state, user_id)?;
        return Ok(Poll::Failed(
            "the sign-in code expired before it was entered; start again".into(),
        ));
    }
    // Move the next allowed poll forward whatever this attempt answers: the
    // provider's interval is a minimum, not a suggestion.
    let interval = std::time::Duration::from_secs(pending.interval.max(1));
    store::schedule_next_poll(state, user_id, now + interval)?;
    let result = match pending.plan {
        Plan::Copilot => copilot::poll_token(&pending, now_ms),
        Plan::Qwen => qwen::poll_token(&pending, now_ms),
        Plan::Kimi => kimi::poll_token(&pending, now_ms),
        _ => return Err("this sign-in does not poll".into()),
    }?;
    if matches!(result, Poll::Done(_)) {
        // Consuming here is what keeps a poll once: the API layer stores the
        // provider from this answer, so a second poll must not find it.
        store::take(state, user_id)?;
    }
    Ok(result)
}

/// New tokens for old (blocking). GitHub Copilot exchanges its GitHub token
/// into a new Copilot token here; everyone else refreshes their own token
/// endpoint.
pub fn refresh(plan: Plan, old: &Credentials, now_ms: i64) -> Result<Credentials, String> {
    match plan {
        Plan::Codex => codex::refresh(old, now_ms),
        Plan::Claude => claude::refresh(old, now_ms),
        Plan::Grok => grok::refresh(old, now_ms),
        Plan::Copilot => copilot::refresh(old, now_ms),
        Plan::Antigravity => antigravity::refresh(old, now_ms),
        Plan::GeminiCli => gemini_cli::refresh(old, now_ms),
        Plan::Qwen => qwen::refresh(old, now_ms),
        Plan::IFlow => iflow::refresh(old, now_ms),
        Plan::Kimi => kimi::refresh(old, now_ms),
    }
}

/// Whether a stored sign-in should re-read its Cloud Code project before the
/// next request: the older Antigravity flow could store the shared
/// `aicode-consumers` project, which generation refuses.
pub fn needs_repair(plan: Plan, credentials: &Credentials) -> bool {
    match plan {
        Plan::Antigravity => antigravity::needs_repair(credentials),
        _ => false,
    }
}

/// Fill in what [`needs_repair`] found (blocking: one request, no onboarding).
/// `Ok(None)` when there was nothing to change.
pub fn repair(
    plan: Plan,
    credentials: &Credentials,
    now_ms: i64,
) -> Result<Option<Credentials>, String> {
    match plan {
        Plan::Antigravity => antigravity::repair(credentials, now_ms),
        _ => Ok(None),
    }
}

/// Read the login the Claude Code CLI already has on this server
/// (`~/.claude/.credentials.json`, or `$CLAUDE_CONFIG_DIR/.credentials.json`)
/// and return its tokens, so an administrator who ran `claude` to sign in
/// need not sign in again in the browser.
pub fn import_claude_code(now_ms: i64) -> Result<Credentials, String> {
    claude_import::import(now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan/kind wiring the settings page and the API rely on: the id is
    /// the kind id, and the inverse mapping finds the plan back.
    #[test]
    fn every_plan_maps_to_its_kind() {
        assert_eq!(Plan::ALL.len(), 9);
        for plan in Plan::ALL {
            let kind = plan.kind();
            assert_eq!(plan.id(), kind.as_str());
            assert_eq!(Plan::of_kind(kind), Some(plan));
            assert!(kind.signs_in(), "{} is in the plan group", plan.id());
            assert!(!plan.label().is_empty());
            assert!(!plan.site().is_empty());
            assert!(
                plan.risk_note().contains("only you can chat with it here"),
                "{} says whose plan it is",
                plan.id()
            );
        }
        // Keyed kinds are not plans (including the API-key qwen/moonshot).
        for kind in [
            Kind::OpenAi,
            Kind::Anthropic,
            Kind::Qwen,
            Kind::Moonshot,
            Kind::Xai,
            Kind::Ollama,
        ] {
            assert_eq!(Plan::of_kind(kind), None);
        }
    }

    /// The three device flows, and the rest browser flows.
    #[test]
    fn each_plan_says_how_it_signs_in() {
        for plan in Plan::ALL {
            let device = matches!(plan, Plan::Copilot | Plan::Qwen | Plan::Kimi);
            assert_eq!(plan.flow() == Flow::Device, device, "{}", plan.id());
            // Only device flows have no address to paste.
            assert_eq!(plan.callback_hint().is_empty(), device, "{}", plan.id());
        }
    }

    /// The pasted address is read leniently: the whole address, just the
    /// query, the bare `code=…&state=…`, and Claude's `code#state`.
    #[test]
    fn the_pasted_answer_is_read() {
        let got = parse_callback(
            " http://localhost:1455/auth/callback?code=ac_1-x%2By&scope=openid&state=abc_D ",
        );
        assert_eq!(
            got,
            Ok(Callback {
                code: "ac_1-x+y".into(),
                state: "abc_D".into()
            })
        );
        assert_eq!(
            parse_callback("code=c1&state=s1"),
            Ok(Callback {
                code: "c1".into(),
                state: "s1".into()
            })
        );
        // Claude's copy-code page shows the state after a #, not in a URL.
        assert_eq!(
            parse_callback("ac_2#st_2"),
            Ok(Callback {
                code: "ac_2".into(),
                state: "st_2".into()
            })
        );
        assert_eq!(
            parse_callback("http://localhost:53692/callback?code=c3&state=s3"),
            Ok(Callback {
                code: "c3".into(),
                state: "s3".into()
            })
        );
        let denied = parse_callback(
            "http://localhost:1455/auth/callback?error=access_denied&error_description=User+cancelled&state=s",
        );
        assert_eq!(
            denied,
            Err("the sign-in was refused: User cancelled".into())
        );
        assert!(parse_callback("https://chatgpt.com/").is_err());
        assert!(parse_callback("http://localhost:1455/auth/callback?state=s").is_err());
        assert!(parse_callback("only-a-code#").is_err());
        assert_eq!(http::decode("a%2"), "a%2");
    }

    /// Pasting is only for browser flows; a device flow's pending is polled.
    #[test]
    fn a_device_flow_has_nothing_to_paste() {
        let pending = Pending::device(
            Plan::Copilot,
            7,
            None,
            "GitHub".into(),
            "device-state".into(),
            "device-code".into(),
            5,
            900,
        );
        assert_eq!(
            finish(&pending, "code", 0),
            Err("this sign-in uses a device code; it finishes by polling, not by pasting".into())
        );
    }

    /// An unknown or someone else's state is refused, and says so clearly.
    #[test]
    fn an_unknown_state_is_not_taken() {
        assert_eq!(
            take_pending("no-such-state", 7).unwrap_err(),
            "this sign-in is not the one started here or it timed out; start again"
        );
        assert_eq!(
            poll("no-such-state", 7, 0).unwrap_err(),
            "this sign-in is not the one started here or it timed out; start again"
        );
    }

    /// Only a device flow is polled; a browser flow's pending says so.
    #[test]
    fn a_browser_flow_is_not_polled() {
        let pending = Pending::browser(
            Plan::Claude,
            7,
            None,
            "Claude".into(),
            "browser-state-for-test".into(),
            "verifier".into(),
            "http://localhost:53692/callback".into(),
        );
        store::insert("browser-state-for-test".into(), pending);
        assert_eq!(
            poll("browser-state-for-test", 7, 0).unwrap_err(),
            "this sign-in finishes with a pasted address, not a poll"
        );
    }
}
