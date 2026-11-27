//! Grok (X Premium) sign-in (a browser flow): the Grok CLI's xAI OAuth
//! client, PKCE, the pasted loopback callback, and the tokens xAI's
//! Responses-style endpoint takes.
//!
//! xAI sends the browser back to `http://127.0.0.1:56121/callback` (the only
//! address this client may use), which a server elsewhere never receives: the
//! person copies that page's address and pastes it into RAST, which checks
//! the `state` and trades the `code` for tokens. The endpoints are xAI's
//! documented OIDC ones (the discovery document serves the same values), so
//! the flow starts without a discovery request.
//!
//! The ID token says who signed in (email, …); the access token's numeric
//! `tier` claim names the X subscription (1 SuperGrok, 3 X Premium, …), which
//! is what the settings page shows as the plan.
//!
//! Access tokens last about an hour; RAST refreshes them ahead of time. The
//! reference clients (9router's xAI service and the DSH Subscriptions plugin)
//! rotate refresh tokens, so a refresh keeps the new one.

use serde_json::Value;

use super::{http, jwt_claims, pkce, store, Credentials, Pending, Plan, Started};

/// The Grok CLI's OAuth client (public; 9router and the DSH plugin use it).
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// xAI's OIDC authorize and token endpoints (the discovery document at
/// `https://auth.x.ai/.well-known/openid-configuration` serves these today;
/// keeping them static is what lets a sign-in start offline).
const AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// Where xAI sends the browser back (fixed for this client).
const REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
/// xAI's CLI mints a 96-byte PKCE verifier (the shared default is 64).
const VERIFIER_BYTES: usize = 96;

/// How long an access token is assumed to last when xAI sends no expiry.
const DEFAULT_ACCESS_SECONDS: i64 = 3600;

/// The X subscription tiers xAI's numeric `tier` claim names (upstream's
/// `SubscriptionTier` enum; a value this list does not know shows as itself).
const TIER_NAMES: [(i64, &str); 8] = [
    (0, "Free"),
    (1, "SuperGrok"),
    (2, "X Basic"),
    (3, "X Premium"),
    (4, "X Premium+"),
    (5, "SuperGrok Heavy"),
    (6, "SuperGrok Lite"),
    (7, "SuperGrok Plus"),
];

/// Start signing in: the address to open in the browser. `state` names the
/// sign-in; the PKCE verifier stays on the server.
pub(crate) fn start(
    user_id: i64,
    provider_id: Option<String>,
    name: String,
    _now_ms: i64,
) -> Result<Started, String> {
    let verifier = pkce::random_url_safe(VERIFIER_BYTES);
    let challenge = pkce::challenge(&verifier);
    let state = pkce::state();
    // xAI's flow carries a nonce and its own `plan` hint; both are opaque
    // here (the callback is read for code and state only).
    let nonce = pkce::random_url_safe(16);
    let url = format!(
        "{AUTHORIZE_URL}?{}",
        http::form(&[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("nonce", &nonce),
            ("plan", "generic"),
        ])
    );
    store::insert(
        state.clone(),
        Pending::browser(
            Plan::Grok,
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

/// The token form for the pasted code. xAI's flow echoes the PKCE challenge
/// and its method alongside the verifier, as the reference clients send them.
fn exchange_form(verifier: &str, redirect_uri: &str, code: &str) -> Vec<(&'static str, String)> {
    let challenge = pkce::challenge(verifier);
    vec![
        ("grant_type", "authorization_code".to_string()),
        ("client_id", CLIENT_ID.to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("code_verifier", verifier.to_string()),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256".to_string()),
    ]
}

/// The token form for a refresh.
fn refresh_form(old: &Credentials) -> Vec<(&'static str, String)> {
    vec![
        ("grant_type", "refresh_token".to_string()),
        ("client_id", CLIENT_ID.to_string()),
        ("refresh_token", old.refresh_token.clone()),
    ]
}

/// Trade the pasted code for tokens (blocking).
pub(crate) fn exchange(pending: &Pending, code: &str, now_ms: i64) -> Result<Credentials, String> {
    let form = exchange_form(&pending.verifier, &pending.redirect_uri, code);
    let body = token_request(&form)?;
    credentials_from(&body, None, now_ms)
}

/// New tokens for old (blocking). The refresh token changes: keep the new one.
pub(crate) fn refresh(old: &Credentials, now_ms: i64) -> Result<Credentials, String> {
    let form = refresh_form(old);
    let body = token_request(&form)?;
    credentials_from(&body, Some(old), now_ms)
}

/// POST one token form and return the JSON answer, or the refusal in xAI's
/// own terms (a 403 means the X plan lacks the API OAuth entitlement).
fn token_request(form: &[(&'static str, String)]) -> Result<Value, String> {
    let pairs: Vec<(&str, &str)> = form
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let reply = http::post_form(TOKEN_URL, "xAI's sign-in", &[], &pairs)?;
    if !(200..300).contains(&reply.status) {
        return Err(token_refusal(reply.status, reply.said()));
    }
    Ok(reply.value)
}

/// What xAI said when it refused a token request.
fn token_refusal(status: u16, said: &str) -> String {
    if status == 403 {
        return "xAI's sign-in refused the exchange (HTTP 403): your X plan does not include the API OAuth entitlement; an X Premium or xAI subscription with API access is required".into();
    }
    if said.is_empty() {
        format!("xAI's sign-in refused it (HTTP {status})")
    } else {
        format!("xAI's sign-in refused it (HTTP {status}): {said}")
    }
}

/// The credentials a token response describes: the tokens, the expiry, and
/// who signed in from its ID (or access) token's claims.
fn credentials_from(
    body: &Value,
    old: Option<&Credentials>,
    now_ms: i64,
) -> Result<Credentials, String> {
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let access_token = text("access_token")
        .ok_or("xAI's sign-in sent no access token")?
        .to_string();
    let refresh_token = text("refresh_token")
        .map(str::to_string)
        .or_else(|| old.map(|o| o.refresh_token.clone()))
        .filter(|token| !token.is_empty())
        .ok_or("xAI's sign-in sent no refresh token")?;
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_ACCESS_SECONDS);
    // Who it is: the ID token's claims, else the access token's.
    let id_claims = text("id_token").and_then(jwt_claims);
    let access_claims = jwt_claims(&access_token);
    let claim = |claims: Option<&Value>, key: &str| {
        claims?
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let email = claim(id_claims.as_ref(), "email")
        .or_else(|| claim(id_claims.as_ref(), "preferred_username"))
        .or_else(|| claim(access_claims.as_ref(), "email"))
        .or_else(|| old.map(|o| o.email.clone()))
        .unwrap_or_default();
    // The tier is on the access token (the reference reads it there); the ID
    // token may carry it too.
    let plan = tier_name(access_claims.as_ref())
        .or_else(|| tier_name(id_claims.as_ref()))
        .or_else(|| old.map(|o| o.plan.clone()))
        .unwrap_or_default();
    Ok(Credentials {
        access_token,
        refresh_token,
        expires_at: now_ms + expires_in * 1000,
        refreshed_at: now_ms,
        // xAI's wire needs no account id with a request.
        account_id: String::new(),
        email,
        plan,
        project_id: old.map(|o| o.project_id.clone()).unwrap_or_default(),
    })
}

/// The subscription name a claims set's numeric `tier` claim means ("X
/// Premium"), or the raw number for a tier this build does not know yet.
fn tier_name(claims: Option<&Value>) -> Option<String> {
    let tier = claims?.get("tier")?;
    if let Some(number) = tier.as_i64() {
        return Some(
            TIER_NAMES
                .iter()
                .find(|(value, _)| *value == number)
                .map(|(_, name)| (*name).to_string())
                .unwrap_or_else(|| number.to_string()),
        );
    }
    tier.as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::signin::{start, take_pending, Started};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde_json::json;
    use sha2::{Digest, Sha256};

    fn jwt(claims: Value) -> String {
        format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    fn credentials() -> Credentials {
        Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 0,
            refreshed_at: 0,
            account_id: String::new(),
            email: String::new(),
            plan: String::new(),
            project_id: String::new(),
        }
    }

    /// The address carries the client, scope, loopback redirect, state and an
    /// S256 challenge for a 96-byte verifier; the sign-in is registered once.
    #[test]
    fn a_sign_in_starts_with_pkce_and_ends_once() {
        let Started::Browser {
            url,
            state,
            redirect_uri,
        } = start(Plan::Grok, 7, None, "Grok".into(), 1_000).unwrap()
        else {
            panic!("Grok signs in with a browser");
        };
        assert_eq!(redirect_uri, REDIRECT_URI);
        assert!(url.starts_with(
            "https://auth.x.ai/oauth2/authorize?response_type=code&client_id=b1a00492-073a-47ea-816f-4c329264a828&"
        ));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A56121%2Fcallback"));
        assert!(url.contains(
            "scope=openid%20profile%20email%20offline_access%20grok-cli%3Aaccess%20api%3Aaccess"
        ));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("plan=generic"));
        assert!(url.contains("nonce="));
        assert!(url.contains(&format!("state={state}")));
        // Only the account that started it, and only once.
        assert!(take_pending(&state, 8).is_err());
        let pending = take_pending(&state, 7).unwrap();
        assert_eq!(pending.plan, Plan::Grok);
        assert_eq!(pending.name, "Grok");
        assert_eq!(pending.provider_id, None);
        // The verifier is xAI's 96 random bytes, base64url.
        let raw = URL_SAFE_NO_PAD.decode(&pending.verifier).unwrap();
        assert_eq!(raw.len(), VERIFIER_BYTES);
        // The challenge in the address is that verifier's SHA-256.
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()));
        assert!(url.contains(&format!("code_challenge={challenge}")));
        assert!(take_pending(&state, 7).is_err());
    }

    /// The exchange echoes the challenge and method with the verifier, as
    /// xAI's flow expects; the refresh names the refresh grant.
    #[test]
    fn the_token_forms_are_xais() {
        let pending = Pending::browser(
            Plan::Grok,
            7,
            None,
            "Grok".into(),
            "state-1".into(),
            "verifier-1".into(),
            REDIRECT_URI.into(),
        );
        let form = exchange_form(&pending.verifier, &pending.redirect_uri, "the-code");
        let names: Vec<&str> = form.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                "grant_type",
                "client_id",
                "code",
                "redirect_uri",
                "code_verifier",
                "code_challenge",
                "code_challenge_method"
            ]
        );
        let value = |key: &str| {
            form.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("grant_type"), Some("authorization_code"));
        assert_eq!(value("client_id"), Some(CLIENT_ID));
        assert_eq!(value("code"), Some("the-code"));
        assert_eq!(value("redirect_uri"), Some(REDIRECT_URI));
        assert_eq!(value("code_verifier"), Some("verifier-1"));
        assert_eq!(
            value("code_challenge"),
            Some(pkce::challenge("verifier-1").as_str())
        );
        assert_eq!(value("code_challenge_method"), Some("S256"));

        let form = refresh_form(&credentials());
        assert_eq!(
            form,
            vec![
                ("grant_type", "refresh_token".to_string()),
                ("client_id", CLIENT_ID.to_string()),
                ("refresh_token", "rt".to_string()),
            ]
        );
    }

    /// A refused token request is worded for the person: a 403 says the X
    /// plan lacks API OAuth, the rest keep xAI's own explanation.
    #[test]
    fn refusals_say_what_xai_said() {
        assert!(token_refusal(403, "").contains("API OAuth entitlement"));
        assert_eq!(
            token_refusal(401, "invalid_grant"),
            "xAI's sign-in refused it (HTTP 401): invalid_grant"
        );
        assert_eq!(
            token_refusal(500, ""),
            "xAI's sign-in refused it (HTTP 500)"
        );
    }

    /// The token response's claims name the account (email) and the X
    /// subscription (numeric tier), and a refresh keeps the rotated-in values.
    #[test]
    fn tokens_say_who_signed_in() {
        let access = jwt(json!({"tier": 3, "email": "someone@example.com"}));
        let id = jwt(json!({"email": "someone@example.com", "sub": "user-1"}));
        let body = json!({
            "access_token": access,
            "refresh_token": "rt",
            "id_token": id,
            "expires_in": 3600,
        });
        let creds = credentials_from(&body, None, 1_000).unwrap();
        assert_eq!(creds.email, "someone@example.com");
        assert_eq!(creds.plan, "X Premium");
        assert_eq!(creds.account_id, "");
        assert_eq!(creds.expires_at, 1_000 + 3_600_000);
        assert!(!creds.needs_refresh(1_000));

        // Every known tier names itself; an unknown number still shows.
        for (tier, name) in [
            (0, "Free"),
            (1, "SuperGrok"),
            (2, "X Basic"),
            (3, "X Premium"),
            (4, "X Premium+"),
            (5, "SuperGrok Heavy"),
            (6, "SuperGrok Lite"),
            (7, "SuperGrok Plus"),
            (42, "42"),
        ] {
            let body = json!({"access_token": jwt(json!({"tier": tier})), "refresh_token": "rt"});
            assert_eq!(
                credentials_from(&body, None, 0).unwrap().plan,
                name,
                "tier {tier}"
            );
        }

        // A refresh without a new refresh token or ID token keeps the old
        // ones, and takes the new access token's tier.
        let again = credentials_from(
            &json!({"access_token": jwt(json!({"tier": 4}))}),
            Some(&creds),
            5_000,
        )
        .unwrap();
        assert_eq!(again.refresh_token, "rt");
        assert_eq!(again.email, "someone@example.com");
        assert_eq!(again.plan, "X Premium+");
        assert_eq!(again.expires_at, 5_000 + 3_600_000);

        // No access token, or no refresh token on a first sign-in, is an
        // error; a missing expiry falls back to an hour.
        assert_eq!(
            credentials_from(&json!({"refresh_token": "rt"}), None, 0).unwrap_err(),
            "xAI's sign-in sent no access token"
        );
        assert_eq!(
            credentials_from(&json!({"access_token": "at"}), None, 0).unwrap_err(),
            "xAI's sign-in sent no refresh token"
        );
        let bare = credentials_from(
            &json!({"access_token": "at", "refresh_token": "rt"}),
            None,
            0,
        )
        .unwrap();
        assert_eq!(bare.expires_at, DEFAULT_ACCESS_SECONDS * 1000);
    }
}
