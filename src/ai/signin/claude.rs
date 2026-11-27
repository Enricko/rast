//! Claude Pro/Max sign-in (a browser flow): the Claude Code OAuth client,
//! PKCE and the pasted callback — the loopback address Claude sends the
//! browser back to, or the `code#state` its copy-code page shows — traded for
//! the tokens Anthropic's Messages API takes.
//!
//! Claude only issues these tokens to its own CLI, so the client id, scopes
//! and endpoints here are the CLI's public ones (the same values 9router and
//! the DSH plugin use), and the wire presents as Claude Code afterwards (see
//! `provider/plans/claude.rs`). The token exchange echoes the sign-in's
//! `state`, which the CLI's endpoint checks.
//!
//! Who signed in (address and plan) comes from the ID token's claims and, when
//! the endpoint sends no usable ones, the OAuth profile endpoint; that lookup
//! is decorative and never fails a sign-in that already has its tokens.

use serde_json::{json, Value};

use super::{http, jwt_claims, pkce, store, Credentials, Pending, Plan, Started};

/// The Claude Code CLI's public OAuth client.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
/// The token endpoint the CLI's flow uses.
const TOKEN_URL: &str = "https://claude.ai/v1/oauth/token";
/// The copy-code page's host answers the same endpoint. Some deployments
/// only answer there, so a 404/405 from claude.ai is retried once here.
const TOKEN_FALLBACK_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Where Claude sends the browser back. The client accepts any loopback
/// port; RAST names a fixed one so the dialog's hint and the exchange agree.
const REDIRECT_URI: &str = "http://localhost:53692/callback";
/// What the CLI asks for: an API key through the console, the profile,
/// inference, and the Claude Code session, MCP and file features.
const SCOPE: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/// The address and plan name ("pro", "max", …) of the signed-in account.
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

/// Start signing in: the address to open in the browser. `state` names the
/// sign-in (and is echoed to the token endpoint); the PKCE verifier stays on
/// the server.
pub(crate) fn start(
    user_id: i64,
    provider_id: Option<String>,
    name: String,
    _now_ms: i64,
) -> Result<Started, String> {
    let verifier = pkce::verifier();
    let challenge = pkce::challenge(&verifier);
    let state = pkce::state();
    // `code=true` is what the CLI sends (both references include it): it tells
    // Claude the answer is a code, not a token, even before the redirect.
    let url = format!(
        "{AUTHORIZE_URL}?{}",
        http::form(&[
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
        ])
    );
    store::insert(
        state.clone(),
        Pending::browser(
            Plan::Claude,
            user_id,
            provider_id,
            name,
            state.clone(),
            verifier,
            REDIRECT_URI.to_string(),
        ),
    );
    Ok(Started::Browser {
        url,
        state,
        redirect_uri: REDIRECT_URI.to_string(),
    })
}

/// The authorization-code grant's body. Kept separate from the request so the
/// wire shape is testable without a network round trip.
fn exchange_body(pending: &Pending, code: &str) -> Value {
    // Claude's copy-code page shows "<code>#<state>". The dialog's lenient
    // parser strips it before this call; strip the suffix here too, and echo
    // the state this sign-in registered (never a value from the paste).
    let code = code.split('#').next().unwrap_or(code).trim();
    json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": pending.redirect_uri,
        "client_id": CLIENT_ID,
        "code_verifier": pending.verifier,
        "state": pending.state,
    })
}

/// The refresh grant's body, which repeats the issued scope (the CLI does).
fn refresh_body(old: &Credentials) -> Value {
    json!({
        "grant_type": "refresh_token",
        "refresh_token": old.refresh_token,
        "client_id": CLIENT_ID,
        "scope": SCOPE,
    })
}

/// POST one grant to Claude's token endpoint, retrying the fallback host when
/// the first address is not there. JSON both ways: Claude's endpoint takes a
/// JSON body, unlike the form posts the other plans use.
fn token_request(body: &Value) -> Result<Value, String> {
    let reply = http::post_json(TOKEN_URL, "Claude's sign-in", &[], body)?;
    let reply = if matches!(reply.status, 404 | 405) {
        http::post_json(TOKEN_FALLBACK_URL, "Claude's sign-in", &[], body)?
    } else {
        reply
    };
    if !(200..300).contains(&reply.status) {
        let said = reply.said();
        return Err(if said.is_empty() {
            format!("Claude's sign-in refused it (HTTP {})", reply.status)
        } else {
            format!(
                "Claude's sign-in refused it (HTTP {}): {said}",
                reply.status
            )
        });
    }
    Ok(reply.value)
}

/// Trade the pasted code for tokens (blocking).
pub(crate) fn exchange(pending: &Pending, code: &str, now_ms: i64) -> Result<Credentials, String> {
    // The exchange learns who signed in from the profile endpoint; a refresh
    // keeps what it already knows instead of asking again (the plugin does
    // the same, and the profile is only decorative).
    let tokens = token_request(&exchange_body(pending, code))?;
    credentials_from(&tokens, None, now_ms, true)
}

/// New tokens for old (blocking). The refresh token rotates: keep the new one.
pub(crate) fn refresh(old: &Credentials, now_ms: i64) -> Result<Credentials, String> {
    let tokens = token_request(&refresh_body(old))?;
    credentials_from(&tokens, Some(old), now_ms, false)
}

/// What the profile endpoint says about the signed-in account.
#[derive(Debug, Default, PartialEq)]
struct Profile {
    account_id: Option<String>,
    email: Option<String>,
    plan: Option<String>,
}

/// One string field of a JSON object, when it says something.
fn field(object: &Value, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Read the profile endpoint's answer. Both shapes the endpoint has used are
/// read: fields at the top level, or under `account`. Pure, so tests need no
/// server.
fn parse_profile(value: &Value) -> Profile {
    let account = value.get("account");
    let nested = |key: &str| account.and_then(|account| field(account, key));
    Profile {
        account_id: nested("uuid").or_else(|| nested("id")),
        email: field(value, "emailAddress")
            .or_else(|| field(value, "email"))
            .or_else(|| nested("email_address"))
            .or_else(|| nested("email")),
        plan: field(value, "subscriptionType")
            .or_else(|| field(value, "subscription_type"))
            .or_else(|| nested("subscription_type")),
    }
}

/// Ask the profile endpoint who signed in. Never an error: the tokens are the
/// sign-in, and the address and plan are decoration for the settings page.
fn profile(access_token: &str) -> Option<Profile> {
    let authorization = format!("Bearer {access_token}");
    let reply = http::get_json(
        PROFILE_URL,
        "Claude's account",
        &[("Authorization", &authorization)],
    )
    .ok()?;
    if !(200..300).contains(&reply.status) {
        return None;
    }
    Some(parse_profile(&reply.value))
}

fn credentials_from(
    body: &Value,
    old: Option<&Credentials>,
    now_ms: i64,
    with_profile: bool,
) -> Result<Credentials, String> {
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let access_token = text("access_token")
        .ok_or("Claude's sign-in sent no access token")?
        .to_string();
    let refresh_token = text("refresh_token")
        .map(str::to_string)
        .or_else(|| old.map(|o| o.refresh_token.clone()))
        .filter(|token| !token.is_empty())
        .ok_or("Claude's sign-in sent no refresh token")?;
    // The endpoint's answer is authoritative; the reference default is an
    // hour when it names no lifetime.
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(3600);
    // Who it is: the ID token's claims when the endpoint sends one, else the
    // access token's (OIDC tokens are signed the same way).
    let claims = text("id_token")
        .and_then(jwt_claims)
        .or_else(|| jwt_claims(&access_token));
    let claim = |key: &str| {
        claims
            .as_ref()
            .and_then(|claims| claims.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let claimed = Profile {
        account_id: claim("sub")
            .or_else(|| claim("account_id"))
            .or_else(|| claim("account_uuid")),
        email: claim("email").or_else(|| claim("email_address")),
        plan: claim("subscriptionType").or_else(|| claim("subscription_type")),
    };
    let profile = if with_profile {
        profile(&access_token).unwrap_or_default()
    } else {
        Profile::default()
    };
    // Claims, then the profile endpoint, then (on a refresh) what the stored
    // sign-in already knew: the exchange and the profile can each lack a bit.
    let keep = |fresh: Option<String>,
                profile: Option<String>,
                old: Option<&Credentials>,
                from_old: fn(&Credentials) -> String| {
        fresh
            .or(profile)
            .or_else(|| old.map(from_old).filter(|value| !value.is_empty()))
            .unwrap_or_default()
    };
    Ok(Credentials {
        access_token,
        refresh_token,
        expires_at: now_ms + expires_in * 1000,
        refreshed_at: now_ms,
        account_id: keep(claimed.account_id, profile.account_id, old, |o| {
            o.account_id.clone()
        }),
        email: keep(claimed.email, profile.email, old, |o| o.email.clone()),
        plan: keep(claimed.plan, profile.plan, old, |o| o.plan.clone()),
        project_id: old.map(|o| o.project_id.clone()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::signin::{start, take_pending, Flow};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use sha2::{Digest, Sha256};

    fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    fn pending() -> Pending {
        Pending::browser(
            Plan::Claude,
            7,
            None,
            "Claude".into(),
            "the-state".into(),
            "verifier-1".into(),
            REDIRECT_URI.into(),
        )
    }

    /// The address opens Claude's own page with the CLI client, the loopback
    /// redirect, the asked-for scopes and an S256 challenge whose verifier
    /// never leaves the server.
    #[test]
    fn a_sign_in_starts_with_pkce_and_ends_once() {
        let Started::Browser {
            url,
            state,
            redirect_uri,
        } = start(Plan::Claude, 7, None, "Claude".into(), 1_000).unwrap()
        else {
            panic!("Claude signs in with a browser");
        };
        assert_eq!(redirect_uri, REDIRECT_URI);
        assert_eq!(Flow::Browser, Plan::Claude.flow());
        assert!(
            url.starts_with("https://claude.ai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&"),
            "{url}"
        );
        assert!(url.contains("response_type=code"), "{url}");
        assert!(
            url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A53692%2Fcallback"),
            "{url}"
        );
        assert!(
            url.contains("scope=org%3Acreate_api_key%20user%3Aprofile%20user%3Ainference%20user%3Asessions%3Aclaude_code%20user%3Amcp_servers%20user%3Afile_upload"),
            "{url}"
        );
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains(&format!("state={state}")), "{url}");
        // Only the person who started it, and only once.
        assert!(take_pending(&state, 8).is_err());
        let pending = take_pending(&state, 7).unwrap();
        assert_eq!(pending.plan, Plan::Claude);
        assert_eq!(pending.name, "Claude");
        assert_eq!(pending.state, state);
        // The challenge in the address is the verifier's SHA-256.
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()));
        assert!(
            url.contains(&format!("code_challenge={challenge}")),
            "{url}"
        );
        assert!(take_pending(&state, 7).is_err());
    }

    /// The exchange repeats this sign-in's redirect, verifier and state; a
    /// raw `code#state` paste is stripped to its code (the registered state
    /// wins).
    #[test]
    fn the_exchange_body_echoes_the_sign_in() {
        let pending = pending();
        let body = exchange_body(&pending, "the-code");
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["client_id"], CLIENT_ID);
        assert_eq!(body["code"], "the-code");
        assert_eq!(body["redirect_uri"], REDIRECT_URI);
        assert_eq!(body["code_verifier"], "verifier-1");
        assert_eq!(body["state"], "the-state");
        let copy_code = exchange_body(&pending, "the-code#a-pasted-state");
        assert_eq!(copy_code["code"], "the-code");
        assert_eq!(copy_code["state"], "the-state");
    }

    /// A refresh asks for new tokens with the old refresh token and the
    /// issued scope.
    #[test]
    fn the_refresh_body_repeats_the_scope() {
        let old = Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 1_000,
            refreshed_at: 0,
            account_id: String::new(),
            email: String::new(),
            plan: String::new(),
            project_id: String::new(),
        };
        let body = refresh_body(&old);
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["refresh_token"], "rt");
        assert_eq!(body["client_id"], CLIENT_ID);
        assert_eq!(body["scope"], SCOPE);
    }

    /// Tokens say who signed in: the ID token's claims carry the address and
    /// plan, and the expiry comes from `expires_in` (an hour when absent).
    #[test]
    fn tokens_say_who_signed_in() {
        let id = jwt(json!({
            "sub": "account-uuid",
            "email": "someone@example.com",
            "subscriptionType": "max"
        }));
        let body = json!({
            "access_token": "at",
            "refresh_token": "rt",
            "id_token": id,
            "expires_in": 3600
        });
        let credentials = credentials_from(&body, None, 1_000, false).unwrap();
        assert_eq!(credentials.access_token, "at");
        assert_eq!(credentials.refresh_token, "rt");
        assert_eq!(credentials.expires_at, 1_000 + 3_600_000);
        assert_eq!(credentials.refreshed_at, 1_000);
        assert_eq!(credentials.account_id, "account-uuid");
        assert_eq!(credentials.email, "someone@example.com");
        assert_eq!(credentials.plan, "max");
        // No named lifetime: an hour.
        let no_expiry = credentials_from(
            &json!({"access_token": "at", "refresh_token": "rt"}),
            None,
            0,
            false,
        )
        .unwrap();
        assert_eq!(no_expiry.expires_at, 3_600_000);
        // A refresh without a new refresh token keeps the old one, and what
        // the exchange learned about the account.
        let again = credentials_from(
            &json!({"access_token": "at2"}),
            Some(&credentials),
            5_000,
            false,
        )
        .unwrap();
        assert_eq!(again.refresh_token, "rt");
        assert_eq!(again.email, "someone@example.com");
        assert_eq!(again.plan, "max");
        assert!(credentials_from(&json!({"refresh_token": "x"}), None, 0, false).is_err());
        assert!(credentials_from(&json!({"access_token": "x"}), None, 0, false).is_err());
    }

    /// The profile endpoint's two shapes: top-level `emailAddress` /
    /// `subscriptionType`, or nested under `account`.
    #[test]
    fn the_profile_is_read_both_ways() {
        let top = json!({
            "account": {"uuid": "uuid-1"},
            "emailAddress": "someone@example.com",
            "subscriptionType": "pro"
        });
        assert_eq!(
            parse_profile(&top),
            Profile {
                account_id: Some("uuid-1".into()),
                email: Some("someone@example.com".into()),
                plan: Some("pro".into()),
            }
        );
        let nested = json!({
            "account": {"id": "uuid-2", "email_address": "other@example.com", "subscription_type": "max"}
        });
        assert_eq!(
            parse_profile(&nested),
            Profile {
                account_id: Some("uuid-2".into()),
                email: Some("other@example.com".into()),
                plan: Some("max".into()),
            }
        );
        // An empty answer says nothing rather than breaking anything.
        assert_eq!(parse_profile(&json!({})), Profile::default());
    }
}
