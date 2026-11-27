//! The parts of a Google plan sign-in that Antigravity and the Gemini CLI
//! share: Google OAuth with PKCE behind the pasted loopback callback, who the
//! Google account is, and the Cloud Code handshake (`loadCodeAssist` /
//! `onboardUser`) that turns the account into the project the generation
//! requests go to.
//!
//! Both clients are the same kind of Google OAuth desktop application: the
//! person opens Google's consent page, the browser comes back to a
//! `http://localhost:…` address (which a server elsewhere never receives, so
//! the person pastes it), and the code is traded for tokens. Google only
//! returns a refresh token with `access_type=offline` and a fresh consent
//! (`prompt=consent`), which RAST always asks for.
//!
//! What differs is who the client says it is: its OAuth application, scopes
//! and return address, and the identity it presents while the Cloud Code
//! backend provisions a project. Those live in each plan's own file; the
//! flow is here once.
//!
//! Both Google plans (`antigravity.rs` and `gemini_cli.rs`) declare their own
//! client identity and reach this module as `super::google`; the flow is here
//! once.

use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{http, pkce, store, Credentials, Pending, Plan, Started};
use crate::ai::provider::cloud_code;

/// Google's consent page.
const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's token endpoint (code exchange and refresh).
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Who the account is; a display detail, so a failure does not stop a sign-in.
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
/// The Cloud Code onboarding host. The Antigravity daily host refuses these
/// calls, so they always go to production (as 9router's executor notes).
const ONBOARD_BASE: &str = "https://cloudcode-pa.googleapis.com";
/// How many times a just-created project is asked about before giving up.
const ONBOARD_ATTEMPTS: u32 = 10;
/// Between onboarding attempts: Google provisions the project in the
/// background, usually within a few seconds.
const ONBOARD_WAIT: Duration = Duration::from_secs(5);

/// Read one Google plan's OAuth desktop client from the server's
/// environment: `RAST_<NAME>_CLIENT_ID` and `RAST_<NAME>_CLIENT_SECRET`
/// (`RAST_ANTIGRAVITY_CLIENT_*`, `RAST_GEMINI_CLI_CLIENT_*`). Google's
/// desktop apps ship their client id and "secret" in every open-source
/// client (9router, pi-ai, the Gemini CLI), but they are still an app's
/// credentials, so RAST reads them from the server (see
/// `docs/CONFIGURATION.md`) instead of keeping them in the repository.
pub(crate) fn client_from_env(name: &str, label: &str) -> Result<(String, String), String> {
    let read = |suffix: &str| {
        std::env::var(format!("RAST_{name}_CLIENT_{suffix}"))
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    match (read("ID"), read("SECRET")) {
        (Some(id), Some(secret)) => Ok((id, secret)),
        _ => Err(format!(
            "{label} sign-in needs its OAuth client: set RAST_{name}_CLIENT_ID and \
             RAST_{name}_CLIENT_SECRET in the server's environment (for example in \
             /etc/rast/rast.env, then restart rast.service) — use the OAuth client the \
             {label} app ships or your own Google OAuth desktop client."
        )),
    }
}

/// What each Google plan's own file says about its sign-in.
pub(crate) struct Config {
    /// The OAuth application, from the server's environment.
    pub client_id: String,
    pub client_secret: String,
    /// The scopes the plan's backend accepts.
    pub scopes: &'static [&'static str],
    /// Where the browser is told to come back (a loopback address the
    /// client registered; the paste flow does not need it to listen).
    pub redirect_uri: &'static str,
    /// How a refusal names the plan in error copy ("Antigravity").
    pub label: &'static str,
    /// What the person can open to finish onboarding ("Antigravity").
    pub app: &'static str,
    /// The identity the onboarding calls present.
    pub identity: Identity,
}

/// How the onboarding calls identify themselves. Google fingerprints them
/// and refuses to provision a project when the wrong client is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Identity {
    /// The Antigravity IDE: its own user agent, and no
    /// `X-Goog-Api-Client`/`Client-Metadata` headers (9router found those
    /// make Google silently decline to create the project).
    Antigravity,
    /// The Gemini CLI's node client: `google-api-nodejs-client` user agent,
    /// the SDK marker and the metadata also as a header.
    GeminiCli,
}

/// A Cloud Code account: the project requests go to and the plan Google says
/// it is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Account {
    project: String,
    plan: String,
}

/// Start signing in: the Google consent page, opened in the browser. Nothing
/// is fetched yet, so this is fast and works offline.
pub(crate) fn start(
    plan: Plan,
    config: &Config,
    user_id: i64,
    provider_id: Option<String>,
    name: String,
    _now_ms: i64,
) -> Result<Started, String> {
    let verifier = pkce::verifier();
    let challenge = pkce::challenge(&verifier);
    let state = pkce::state();
    let url = authorize_url(config, &challenge, &state);
    store::insert(
        state.clone(),
        Pending::browser(
            plan,
            user_id,
            provider_id,
            name,
            state.clone(),
            verifier,
            config.redirect_uri.to_string(),
        ),
    );
    Ok(Started::Browser {
        url,
        state,
        redirect_uri: config.redirect_uri.to_string(),
    })
}

/// The consent page's address: offline access and a fresh consent so Google
/// returns a refresh token, with the PKCE challenge and our `state`.
fn authorize_url(config: &Config, challenge: &str, state: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?{}",
        http::form(&[
            ("access_type", "offline"),
            ("client_id", config.client_id.as_str()),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("include_granted_scopes", "true"),
            ("prompt", "consent"),
            ("redirect_uri", config.redirect_uri),
            ("response_type", "code"),
            ("scope", &config.scopes.join(" ")),
            ("state", state),
        ])
    )
}

/// The code exchange's form fields.
fn exchange_pairs<'a>(
    config: &'a Config,
    pending: &'a Pending,
    code: &'a str,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &pending.redirect_uri),
        ("client_id", config.client_id.as_str()),
        ("code_verifier", &pending.verifier),
        ("client_secret", config.client_secret.as_str()),
    ]
}

/// The refresh's form fields.
fn refresh_pairs<'a>(config: &'a Config, refresh_token: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", config.client_id.as_str()),
        ("client_secret", config.client_secret.as_str()),
    ]
}

/// Trade the pasted code for tokens and set the account up for Cloud Code
/// (blocking: the API layer runs this on `spawn_blocking`).
pub(crate) fn exchange(
    config: &Config,
    pending: &Pending,
    code: &str,
    now_ms: i64,
) -> Result<Credentials, String> {
    let pairs = exchange_pairs(config, pending, code);
    let reply = http::post_form(TOKEN_URL, "Google's sign-in", &[], &pairs)?;
    let tokens = token_body(config, reply)?;
    let access_token = text(&tokens, "access_token")
        .ok_or_else(|| format!("Google's sign-in sent {} no access token", config.label))?;
    let email = user_info(access_token);
    let account = discover_account(config, access_token, &email)?;
    credentials_from(&tokens, config, account, email, now_ms)
}

/// The credentials a token answer holds: the access token, the refresh token
/// (required on the first exchange), its lifetime and the Cloud Code account.
fn credentials_from(
    tokens: &Value,
    config: &Config,
    account: Account,
    email: String,
    now_ms: i64,
) -> Result<Credentials, String> {
    let access_token = text(tokens, "access_token")
        .ok_or_else(|| format!("Google's sign-in sent {} no access token", config.label))?;
    let refresh_token = text(tokens, "refresh_token").ok_or_else(|| {
        "Google's sign-in sent no refresh token; remove RAST's access at myaccount.google.com/permissions, then sign in again"
            .to_string()
    })?;
    Ok(Credentials {
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at: now_ms + expires_in(tokens) * 1000,
        refreshed_at: now_ms,
        account_id: account.project.clone(),
        email,
        plan: account.plan,
        project_id: account.project,
    })
}

/// New tokens for old (blocking). Google does not repeat the project or who
/// the person is on a refresh, so those are kept from the stored sign-in.
pub(crate) fn refresh(
    config: &Config,
    old: &Credentials,
    now_ms: i64,
) -> Result<Credentials, String> {
    let pairs = refresh_pairs(config, &old.refresh_token);
    let reply = http::post_form(TOKEN_URL, "Google's sign-in", &[], &pairs)?;
    let tokens = token_body(config, reply)?;
    refreshed_from(&tokens, config, old, now_ms)
}

/// The credentials a refresh answer holds. Google does not repeat the project
/// or who the person is, and rotates the refresh token only sometimes, so the
/// stored sign-in fills the gaps.
fn refreshed_from(
    tokens: &Value,
    config: &Config,
    old: &Credentials,
    now_ms: i64,
) -> Result<Credentials, String> {
    let access_token = text(tokens, "access_token").ok_or_else(|| {
        format!(
            "Google's token refresh sent {} no access token",
            config.label
        )
    })?;
    let refresh_token = text(tokens, "refresh_token").unwrap_or(&old.refresh_token);
    // The project is what signed_in_token hands to the wire as the account.
    let project = if old.project_id.is_empty() {
        old.account_id.clone()
    } else {
        old.project_id.clone()
    };
    Ok(Credentials {
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at: now_ms + expires_in(tokens) * 1000,
        refreshed_at: now_ms,
        account_id: project.clone(),
        email: old.email.clone(),
        plan: old.plan.clone(),
        project_id: project,
    })
}

/// A non-empty string field of a token answer.
fn text<'a>(tokens: &'a Value, key: &str) -> Option<&'a str> {
    tokens
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// The token answer's lifetime in seconds (Google's ~3600; a missing one is
/// not a reason to fail: RAST refreshes ahead of time anyway).
fn expires_in(tokens: &Value) -> i64 {
    tokens
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(3600)
}

/// Check the token endpoint's answer; a refusal is worded with the plan's
/// name (never with the token material).
fn token_body(config: &Config, reply: http::Reply) -> Result<Value, String> {
    if (200..300).contains(&reply.status) {
        return Ok(reply.value);
    }
    let said = reply.said();
    Err(if said.is_empty() {
        format!(
            "Google refused {}'s sign-in (HTTP {})",
            config.label, reply.status
        )
    } else {
        format!(
            "Google refused {}'s sign-in (HTTP {}): {said}",
            config.label, reply.status
        )
    })
}

/// The account's email, for display only: when Google does not answer, the
/// sign-in still stands (the project is the login boundary).
fn user_info(access_token: &str) -> String {
    let authorization = format!("Bearer {access_token}");
    let Ok(reply) = http::get_json(
        USERINFO_URL,
        "Google's sign-in",
        &[("Authorization", authorization.as_str())],
    ) else {
        return String::new();
    };
    if !(200..300).contains(&reply.status) {
        return String::new();
    }
    reply
        .value
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Read the Cloud Code project and plan from `loadCodeAssist`, then make sure
/// the project is one the account can actually use.
///
/// Google answers the shared consumer project (`aicode-consumers`) for
/// accounts it has not provisioned a Cloud Code project for; generation with
/// that project is refused (the caller lacks `serviceusage.services.use` on
/// it). So: onboard when the answer is empty or shared — a fresh project is
/// created for the account — and, for Antigravity, fall back to a stable
/// per-account project id when Google still offers only the shared one (the
/// current clients send such an id when discovery finds nothing).
fn discover_account(config: &Config, access_token: &str, seed: &str) -> Result<Account, String> {
    let load = call_method(
        config,
        "loadCodeAssist",
        &load_body(config.identity),
        access_token,
    )?;
    let plan = plan_of(&load);
    let mut project = project_of(&load);
    if project.is_empty() || is_shared_project(&project) {
        if let Ok(found) = onboard(config, access_token, &default_tier(&load)) {
            if !found.is_empty() && !is_shared_project(&found) {
                project = found;
            }
        }
    }
    if (project.is_empty() || is_shared_project(&project))
        && config.identity == Identity::Antigravity
    {
        project = synthetic_project(seed);
    }
    if project.is_empty() || is_shared_project(&project) {
        return Err(no_project_error(config));
    }
    Ok(Account { project, plan })
}

/// Google's shared consumer project: usable for the consumer clients'
/// metadata calls, but generation against it is refused for a normal account.
const SHARED_CONSUMER_PROJECT: &str = "aicode-consumers";

fn is_shared_project(project: &str) -> bool {
    project.trim().eq_ignore_ascii_case(SHARED_CONSUMER_PROJECT)
}

/// A stable project id for an account Google would not provision one for
/// (`<uuid>` shaped, derived from the account's email): the current
/// Antigravity clients send such an id, and the backend accepts it for
/// generation. Empty seeds still get a deterministic id.
fn synthetic_project(seed: &str) -> String {
    let seed = seed.trim();
    let seed = if seed.is_empty() {
        "antigravity-default"
    } else {
        seed
    };
    let digest = Sha256::digest(format!("antigravity:{seed}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // UUID version 5-ish and RFC 4122 variant, as the reference derives it.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Re-read the project for a sign-in that already has tokens (no onboarding:
/// it was attempted at sign-in, and a first chat must not wait for it). A
/// project Google will not provision (empty or the shared consumer one) falls
/// back to the account's stable synthetic id.
pub(crate) fn repair_project(config: &Config, access_token: &str, seed: &str) -> String {
    let load = call_method(
        config,
        "loadCodeAssist",
        &load_body(config.identity),
        access_token,
    );
    if let Ok(load) = load {
        let project = project_of(&load);
        if !project.is_empty() && !is_shared_project(&project) {
            return project;
        }
    }
    synthetic_project(seed)
}

/// What a person can do when Google has no project for the account.
fn no_project_error(config: &Config) -> String {
    format!(
        "Google did not give this account a Cloud Code project; open {} and finish its onboarding, then sign in again",
        config.app
    )
}

/// `onboardUser` until Google says the project is ready (a fresh project is
/// provisioned in the background; the retries are bounded so a sign-in cannot
/// hang forever).
fn onboard(config: &Config, access_token: &str, tier: &str) -> Result<String, String> {
    for attempt in 0..ONBOARD_ATTEMPTS {
        let reply = call_method(
            config,
            "onboardUser",
            &onboard_body(config.identity, tier),
            access_token,
        )?;
        if reply.get("done").and_then(Value::as_bool) == Some(true) {
            let project = reply.get("response").map(project_of).unwrap_or_default();
            if !project.is_empty() {
                return Ok(project);
            }
        }
        if attempt + 1 < ONBOARD_ATTEMPTS {
            std::thread::sleep(ONBOARD_WAIT);
        }
    }
    Err(
        "Google did not finish setting up the Cloud Code project in time; try signing in again"
            .into(),
    )
}

/// The `loadCodeAssist` request: each client presents its own metadata, and
/// the Gemini CLI's node client also sends `mode: 1`, which 9router's flow
/// carries; the Antigravity client does not.
fn load_body(identity: Identity) -> Value {
    match identity {
        Identity::Antigravity => json!({ "metadata": cloud_code::antigravity_metadata() }),
        Identity::GeminiCli => json!({ "metadata": cloud_code::metadata(), "mode": 1 }),
    }
}

/// The `onboardUser` request, with the client's own metadata.
fn onboard_body(identity: Identity, tier: &str) -> Value {
    let metadata = match identity {
        Identity::Antigravity => cloud_code::antigravity_metadata(),
        Identity::GeminiCli => cloud_code::metadata(),
    };
    json!({
        "tierId": tier,
        "metadata": metadata,
    })
}

/// `POST {Cloud Code}/v1internal:{method}` with the client's onboarding
/// identity and the OAuth bearer.
fn call_method(
    config: &Config,
    method: &str,
    body: &Value,
    access_token: &str,
) -> Result<Value, String> {
    let url = format!("{ONBOARD_BASE}/v1internal:{method}");
    let authorization = format!("Bearer {access_token}");
    let user_agent;
    let api_client;
    let metadata;
    let mut headers: Vec<(&str, &str)> = vec![
        ("Content-Type", "application/json"),
        ("Authorization", authorization.as_str()),
    ];
    match config.identity {
        Identity::Antigravity => {
            user_agent = cloud_code::ANTIGRAVITY_USER_AGENT;
            headers.push(("User-Agent", user_agent));
        }
        Identity::GeminiCli => {
            user_agent = cloud_code::ONBOARDING_USER_AGENT;
            api_client = cloud_code::ONBOARDING_API_CLIENT;
            metadata = cloud_code::metadata().to_string();
            headers.push(("User-Agent", user_agent));
            headers.push(("X-Goog-Api-Client", api_client));
            headers.push(("Client-Metadata", metadata.as_str()));
        }
    }
    let reply = http::post_json(&url, "Google's Cloud Code", &headers, body)?;
    if !(200..300).contains(&reply.status) {
        let said = reply.said();
        return Err(if said.is_empty() {
            format!(
                "Google could not set up {} (HTTP {})",
                config.label, reply.status
            )
        } else {
            format!(
                "Google could not set up {} (HTTP {}): {said}",
                config.label, reply.status
            )
        });
    }
    Ok(reply.value)
}

/// The project `loadCodeAssist`/`onboardUser` answers with. Google has used
/// several field names for it and both a bare id and an `{id}` object; the
/// current clients read all of them (pi-antigravity's `extractProjectId`).
fn project_of(value: &Value) -> String {
    for key in [
        "antigravityProjectId",
        "projectId",
        "backendProjectId",
        "userDefinedCloudaicompanionProject",
        "cloudaicompanionProject",
        "project",
    ] {
        if let Some(project) = project_string(value.get(key)) {
            return project;
        }
    }
    for key in ["projects", "projectIds", "cloudaicompanionProjects"] {
        if let Some(items) = value.get(key).and_then(Value::as_array) {
            for item in items {
                if let Some(project) = project_string(Some(item)) {
                    return project;
                }
            }
        }
    }
    String::new()
}

/// One project field: a bare id or an `{id}` object.
fn project_string(value: Option<&Value>) -> Option<String> {
    let project = match value? {
        Value::String(project) => project.trim().to_string(),
        Value::Object(fields) => fields
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        _ => String::new(),
    };
    if project.is_empty() {
        None
    } else {
        Some(project)
    }
}

/// The plan Google reports: the paid tier first, then the current one (its
/// name is friendlier but an id is equally usable).
fn plan_of(load: &Value) -> String {
    let tier = |tier: &Value| {
        tier.get("name")
            .and_then(Value::as_str)
            .or_else(|| tier.get("id").and_then(Value::as_str))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    };
    load.get("paidTier")
        .and_then(tier)
        .or_else(|| load.get("currentTier").and_then(tier))
        .unwrap_or_default()
}

/// The tier to onboard with: the default the account is allowed (else the
/// legacy tier 9router falls back to).
fn default_tier(load: &Value) -> String {
    load.get("allowedTiers")
        .and_then(Value::as_array)
        .and_then(|tiers| {
            tiers
                .iter()
                .find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))
                .and_then(|tier| tier.get("id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "legacy-tier".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            client_id: "client-1.example.com".to_string(),
            client_secret: "secret-1".to_string(),
            scopes: &["openid", "https://www.googleapis.com/auth/cloud-platform"],
            redirect_uri: "http://localhost:56123/oauth-callback",
            label: "Test Plan",
            app: "the Test IDE",
            identity: Identity::Antigravity,
        }
    }

    fn pending() -> Pending {
        Pending::browser(
            Plan::Antigravity,
            7,
            None,
            "Google".into(),
            "state-1".into(),
            "verifier-1".into(),
            "http://localhost:56123/oauth-callback".into(),
        )
    }

    /// The consent page: offline access with a fresh consent, PKCE S256, the
    /// plan's client/scopes/return address and the sign-in's state.
    #[test]
    fn the_consent_page_asks_for_offline_pkce() {
        let url = authorize_url(&config(), "challenge_1", "state_1");
        assert!(
            url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"),
            "{url}"
        );
        for part in [
            "access_type=offline",
            "code_challenge=challenge_1",
            "code_challenge_method=S256",
            "include_granted_scopes=true",
            "prompt=consent",
            "client_id=client-1.example.com",
            "redirect_uri=http%3A%2F%2Flocalhost%3A56123%2Foauth-callback",
            "response_type=code",
            "scope=openid%20https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform",
            "state=state_1",
        ] {
            assert!(url.contains(part), "missing {part} in {url}");
        }
    }

    /// The code exchange repeats the redirect and presents the PKCE verifier
    /// and the client's secret; the refresh presents the stored refresh token.
    #[test]
    fn the_token_requests_carry_the_client_and_pkce() {
        let pending = pending();
        let config = config();
        let pairs = exchange_pairs(&config, &pending, "the-code");
        let has = |name: &str, value: &str| pairs.iter().any(|(k, v)| *k == name && *v == value);
        assert!(has("grant_type", "authorization_code"));
        assert!(has("code", "the-code"));
        assert!(has("redirect_uri", "http://localhost:56123/oauth-callback"));
        assert!(has("client_id", "client-1.example.com"));
        assert!(has("code_verifier", "verifier-1"));
        assert!(has("client_secret", "secret-1"));
        let pairs = refresh_pairs(&config, "refresh-1");
        let has = |name: &str, value: &str| pairs.iter().any(|(k, v)| *k == name && *v == value);
        assert!(has("grant_type", "refresh_token"));
        assert!(has("refresh_token", "refresh-1"));
        assert!(has("client_id", "client-1.example.com"));
        assert!(has("client_secret", "secret-1"));
    }

    /// The Cloud Code handshake's request bodies: Antigravity's minimal
    /// metadata, the Gemini CLI's numeric metadata with `mode: 1`, and the
    /// default tier.
    #[test]
    fn the_handshake_bodies_are_cloud_codes() {
        let load = load_body(Identity::Antigravity);
        assert_eq!(
            load,
            json!({"metadata": {"ideType": "ANTIGRAVITY"}}),
            "{load}"
        );
        assert!(load.get("mode").is_none());
        let load = load_body(Identity::GeminiCli);
        assert_eq!(load["metadata"]["ideType"], 9);
        assert_eq!(load["metadata"]["pluginType"], 2);
        assert!(load["metadata"]["platform"].is_number());
        assert_eq!(load["mode"], 1);
        let onboard = onboard_body(Identity::Antigravity, "free-tier");
        assert_eq!(onboard["tierId"], "free-tier");
        assert_eq!(onboard["metadata"]["ideType"], "ANTIGRAVITY");
        let onboard = onboard_body(Identity::GeminiCli, "free-tier");
        assert_eq!(onboard["metadata"]["ideType"], 9);
    }

    /// The project shapes, the plan tiers and the default tier are read as
    /// the current clients read them (several field names, a bare id or an
    /// `{id}` object, plus the list forms).
    #[test]
    fn the_project_plan_and_tier_are_read() {
        assert_eq!(
            project_of(&json!({"cloudaicompanionProject": "project-1"})),
            "project-1"
        );
        assert_eq!(
            project_of(&json!({"cloudaicompanionProject": {"id": " project-2 "}})),
            "project-2"
        );
        assert_eq!(
            project_of(&json!({"antigravityProjectId": " project-3 "})),
            "project-3"
        );
        assert_eq!(project_of(&json!({"projectId": "project-4"})), "project-4");
        assert_eq!(
            project_of(&json!({"cloudaicompanionProjects": [{"id": "project-5"}]})),
            "project-5"
        );
        assert_eq!(project_of(&json!({})), "");
        assert_eq!(
            plan_of(&json!({"paidTier": {"id": "tier-1", "name": "Pro"}})),
            "Pro"
        );
        assert_eq!(
            plan_of(&json!({"currentTier": {"id": "free-tier"}})),
            "free-tier"
        );
        assert_eq!(plan_of(&json!({})), "");
        assert_eq!(
            default_tier(&json!({"allowedTiers": [
                {"id": "other"},
                {"id": " free-tier ", "isDefault": true},
            ]})),
            "free-tier"
        );
        assert_eq!(default_tier(&json!({})), "legacy-tier");
    }

    /// Google's shared consumer project is never used: generation against it
    /// is refused for a normal account. Antigravity falls back to a stable
    /// per-account id instead (the current clients send one too).
    #[test]
    fn the_shared_consumer_project_is_not_used() {
        assert!(is_shared_project("aicode-consumers"));
        assert!(is_shared_project(" AICode-Consumers "));
        assert!(!is_shared_project("gen-lang-client-1"));
        let first = synthetic_project("someone@example.com");
        assert_eq!(first, synthetic_project("someone@example.com"));
        assert_ne!(first, synthetic_project("other@example.com"));
        assert_ne!(first, synthetic_project(""));
        assert_eq!(first.len(), 36, "{first}");
        let pieces: Vec<&str> = first.split('-').collect();
        assert_eq!(
            pieces.iter().map(|piece| piece.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
    }

    /// A refused token exchange and a refused onboarding are worded with the
    /// plan's name and Google's own explanation, never with token material.
    #[test]
    fn refusals_are_worded_without_secrets() {
        let refused = token_body(
            &config(),
            http::Reply {
                status: 400,
                value: json!({
                    "error": "invalid_grant",
                    "error_description": "Bad Request",
                }),
            },
        )
        .unwrap_err();
        assert!(
            refused.contains("Google refused Test Plan's sign-in (HTTP 400): Bad Request"),
            "{refused}"
        );
        assert!(!refused.contains("secret"), "{refused}");
        assert!(token_body(
            &config(),
            http::Reply {
                status: 400,
                value: Value::Null,
            },
        )
        .unwrap_err()
        .contains("Google refused Test Plan's sign-in (HTTP 400)"));
        // A 2xx answer passes through.
        assert_eq!(
            token_body(
                &config(),
                http::Reply {
                    status: 200,
                    value: json!({"access_token": "at"}),
                },
            )
            .unwrap()["access_token"],
            "at"
        );
        assert!(no_project_error(&config()).contains("the Test IDE"));
    }

    /// The token answer becomes credentials: the project is both the account
    /// the wire sends and the stored project id, and a refused answer without
    /// a refresh token (or without an access token) is an error.
    #[test]
    fn tokens_become_credentials() {
        let tokens = json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "expires_in": 3599,
        });
        let account = Account {
            project: "project-1".into(),
            plan: "Pro".into(),
        };
        let credentials = credentials_from(
            &tokens,
            &config(),
            account,
            "someone@example.com".into(),
            1_000,
        )
        .unwrap();
        assert_eq!(credentials.access_token, "access-1");
        assert_eq!(credentials.refresh_token, "refresh-1");
        assert_eq!(credentials.expires_at, 1_000 + 3_599_000);
        assert_eq!(credentials.refreshed_at, 1_000);
        assert_eq!(credentials.account_id, "project-1");
        assert_eq!(credentials.project_id, "project-1");
        assert_eq!(credentials.email, "someone@example.com");
        assert_eq!(credentials.plan, "Pro");
        // A first exchange without a refresh token cannot be refreshed later.
        let no_refresh = json!({"access_token": "access-2"});
        let error = credentials_from(
            &no_refresh,
            &config(),
            Account {
                project: "project-1".into(),
                plan: String::new(),
            },
            String::new(),
            0,
        )
        .unwrap_err();
        assert!(error.contains("no refresh token"), "{error}");
        assert!(
            error.contains("myaccount.google.com/permissions"),
            "{error}"
        );
        // And one without an access token cannot be used at all.
        assert!(credentials_from(
            &json!({"refresh_token": "refresh-2"}),
            &config(),
            Account {
                project: "project-1".into(),
                plan: String::new(),
            },
            String::new(),
            0,
        )
        .unwrap_err()
        .contains("no access token"));
    }

    /// A refresh keeps the project, the email and the plan (Google does not
    /// repeat them), rotates the refresh token only when it sends one, and
    /// falls back to the account id for a sign-in stored before the project
    /// had its own field.
    #[test]
    fn a_refresh_keeps_what_google_does_not_repeat() {
        let old = Credentials {
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            expires_at: 0,
            refreshed_at: 0,
            account_id: "project-1".into(),
            email: "someone@example.com".into(),
            plan: "Pro".into(),
            project_id: "project-1".into(),
        };
        let refreshed = refreshed_from(
            &json!({"access_token": "new-access", "expires_in": 3599}),
            &config(),
            &old,
            5_000,
        )
        .unwrap();
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token, "old-refresh");
        assert_eq!(refreshed.account_id, "project-1");
        assert_eq!(refreshed.email, "someone@example.com");
        assert_eq!(refreshed.plan, "Pro");
        assert_eq!(refreshed.expires_at, 5_000 + 3_599_000);
        assert_eq!(refreshed.refreshed_at, 5_000);
        // A rotated refresh token is kept.
        let rotated = refreshed_from(
            &json!({"access_token": "new-access", "refresh_token": "new-refresh"}),
            &config(),
            &old,
            5_000,
        )
        .unwrap();
        assert_eq!(rotated.refresh_token, "new-refresh");
        assert_eq!(
            rotated.expires_at,
            5_000 + 3_600_000,
            "a missing lifetime is an hour"
        );
        // An old row without a project id falls back to its account id.
        let mut legacy = old;
        legacy.project_id = String::new();
        let fallback =
            refreshed_from(&json!({"access_token": "a"}), &config(), &legacy, 0).unwrap();
        assert_eq!(fallback.account_id, "project-1");
        assert_eq!(fallback.project_id, "project-1");
    }
}
