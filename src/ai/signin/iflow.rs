//! iFlow sign-in (a browser flow): iFlow's authorization-code flow with the
//! pasted callback in place of 9router's localhost forwarding trick.
//!
//! iFlow sends the browser back to `http://localhost:56124/oauth2callback`,
//! which a server elsewhere never receives: the person copies that page's
//! address and pastes it into RAST, which checks the `state` and trades the
//! `code` for tokens. iFlow's OAuth client is confidential (it pairs a client
//! id with a secret), so the flow has no PKCE — the secret and the `state`
//! are the protections, exactly as in 9router's service.
//!
//! One iFlow peculiarity shapes the stored credentials: the OAuth access
//! token is only good for asking who signed in. The API itself
//! (`apis.iflow.cn`) takes the *API key* that
//! `https://iflow.cn/api/oauth/getUserInfo` returns for the account, so the
//! API key is what RAST stores as the provider's access token (the wire
//! sends it as the bearer) and also as the account id, because iFlow's
//! requests are signed with an HMAC whose key is that API key (see
//! `provider/plans/iflow.rs`). The OAuth access token is not kept: a refresh
//! mints a fresh one, asks iFlow who the account is again, and replaces the
//! API key if it changed.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;

use super::{http, pkce, store, Credentials, Pending, Plan, Started};

/// iFlow's OAuth client: its app id and a secret that signs every request.
/// Like Google's desktop clients it is an application credential, not a
/// person's, so RAST reads it from the server's environment
/// (`RAST_IFLOW_CLIENT_ID` / `RAST_IFLOW_CLIENT_SECRET`; see
/// `docs/CONFIGURATION.md`) instead of keeping it in the repository.
const AUTHORIZE_URL: &str = "https://iflow.cn/oauth";
const TOKEN_URL: &str = "https://iflow.cn/oauth/token";
const USERINFO_URL: &str = "https://iflow.cn/api/oauth/getUserInfo";
/// Where iFlow sends the browser back: the port 9router's local server used,
/// kept so the pasted address looks like the one its CLI produces.
const REDIRECT_URI: &str = "http://localhost:56124/oauth2callback";
/// iFlow's page asks which login form to show; its own CLI asks for the phone
/// one.
const LOGIN_METHOD: &str = "phone";
const SITE: &str = "iFlow's sign-in";

/// The OAuth client from the server's environment.
fn client() -> Result<(String, String), String> {
    let read = |suffix: &str| {
        std::env::var(format!("RAST_IFLOW_CLIENT_{suffix}"))
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    match (read("ID"), read("SECRET")) {
        (Some(id), Some(secret)) => Ok((id, secret)),
        _ => Err(
            "iFlow sign-in needs its OAuth client: set RAST_IFLOW_CLIENT_ID and \
                  RAST_IFLOW_CLIENT_SECRET in the server's environment (for example in \
                  /etc/rast/rast.env, then restart rast.service)"
                .into(),
        ),
    }
}

/// The longest tokens live when iFlow does not say (they last much longer;
/// an early refresh costs one request and keeps the session fresh).
const DEFAULT_EXPIRES_IN: i64 = 3600;

/// Start signing in: the address to open in the browser. iFlow's flow has no
/// PKCE, so only the random `state` ties the pasted answer to this sign-in.
pub(crate) fn start(
    user_id: i64,
    provider_id: Option<String>,
    name: String,
    _now_ms: i64,
) -> Result<Started, String> {
    let (client_id, _secret) = client()?;
    let state = pkce::state();
    let url = format!(
        "{AUTHORIZE_URL}?{}",
        http::form(&[
            ("loginMethod", LOGIN_METHOD),
            ("type", LOGIN_METHOD),
            // iFlow names the callback `redirect`, not `redirect_uri`.
            ("redirect", REDIRECT_URI),
            ("state", state.as_str()),
            ("client_id", client_id.as_str()),
        ])
    );
    store::insert(
        state.clone(),
        Pending::browser(
            Plan::IFlow,
            user_id,
            provider_id,
            name,
            state.clone(),
            // No PKCE: iFlow's flow does not take one.
            String::new(),
            REDIRECT_URI.to_string(),
        ),
    );
    Ok(Started::Browser {
        url,
        state,
        redirect_uri: REDIRECT_URI.to_string(),
    })
}

/// `Authorization: Basic base64(client_id:client_secret)` — iFlow takes the
/// client's secret both there and in the form body.
fn basic_auth(client_id: &str, client_secret: &str) -> String {
    format!(
        "Basic {}",
        STANDARD.encode(format!("{client_id}:{client_secret}"))
    )
}

/// The form that trades the pasted code for tokens.
fn token_pairs<'a>(
    client_id: &'a str,
    client_secret: &'a str,
    pending: &'a Pending,
    code: &'a str,
) -> Vec<(&'static str, &'a str)> {
    vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", pending.redirect_uri.as_str()),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ]
}

/// The form that mints new tokens from a refresh token.
fn refresh_pairs<'a>(
    client_id: &'a str,
    client_secret: &'a str,
    old: &'a Credentials,
) -> Vec<(&'static str, &'a str)> {
    vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", old.refresh_token.as_str()),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ]
}

/// Where iFlow's user information (with the account's API key) is asked for:
/// the OAuth access token goes in the query, as iFlow's own CLI sends it.
fn userinfo_url(access_token: &str) -> String {
    format!(
        "{USERINFO_URL}?{}",
        http::form(&[("accessToken", access_token)])
    )
}

/// Trade the pasted code for tokens, then ask iFlow who signed in for the
/// account's API key (blocking).
pub(crate) fn exchange(pending: &Pending, code: &str, now_ms: i64) -> Result<Credentials, String> {
    let (client_id, client_secret) = client()?;
    let pairs = token_pairs(&client_id, &client_secret, pending, code);
    let authorization = basic_auth(&client_id, &client_secret);
    let headers = [("Authorization", authorization.as_str())];
    let tokens = token_request(&pairs, &headers)?;
    let access_token =
        text(&tokens, "access_token").ok_or("iFlow's sign-in sent no access token")?;
    let (api_key, email) = user_info(access_token)?;
    credentials(&tokens, &api_key, &email, None, now_ms)
}

/// New tokens for old (blocking): refresh the OAuth session, ask who the
/// account is again (the API key can be reissued), and keep the same API key
/// if that second request fails.
pub(crate) fn refresh(old: &Credentials, now_ms: i64) -> Result<Credentials, String> {
    let (client_id, client_secret) = client()?;
    let pairs = refresh_pairs(&client_id, &client_secret, old);
    let authorization = basic_auth(&client_id, &client_secret);
    let headers = [("Authorization", authorization.as_str())];
    let tokens = token_request(&pairs, &headers)?;
    let access_token =
        text(&tokens, "access_token").ok_or("iFlow's sign-in sent no access token")?;
    let (api_key, email) =
        user_info(access_token).unwrap_or_else(|_| (old.access_token.clone(), old.email.clone()));
    credentials(&tokens, &api_key, &email, Some(old), now_ms)
}

fn token_request(pairs: &[(&str, &str)], headers: &[(&str, &str)]) -> Result<Value, String> {
    let reply = http::post_form(TOKEN_URL, SITE, headers, pairs)?;
    if !(200..300).contains(&reply.status) {
        return Err(refused(reply.status, reply.said()));
    }
    Ok(reply.value)
}

fn user_info(access_token: &str) -> Result<(String, String), String> {
    let reply = http::get_json(&userinfo_url(access_token), SITE, &[])?;
    if !(200..300).contains(&reply.status) {
        return Err(format!(
            "iFlow did not say who signed in (HTTP {})",
            reply.status
        ));
    }
    parse_user_info(&reply.value)
}

/// iFlow's answer to the user-information request: `{success, data:{apiKey,
/// email, phone, …}}`. The API key is required — the chat endpoint does not
/// take the OAuth token.
fn parse_user_info(value: &Value) -> Result<(String, String), String> {
    let said = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    };
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        let message = said("message")
            .or_else(|| said("error_description"))
            .unwrap_or("");
        return Err(if message.is_empty() {
            "iFlow did not say who signed in".into()
        } else {
            format!("iFlow did not say who signed in: {message}")
        });
    }
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    let api_key = data
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or("iFlow sent no API key for this account; sign in again")?;
    let email = ["email", "phone"]
        .iter()
        .find_map(|key| {
            data.get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("")
        .to_string();
    Ok((api_key.to_string(), email))
}

/// The tokens and who they are: `api_key` becomes both the wire's bearer and
/// the signing key, the OAuth access token is deliberately dropped.
fn credentials(
    tokens: &Value,
    api_key: &str,
    email: &str,
    old: Option<&Credentials>,
    now_ms: i64,
) -> Result<Credentials, String> {
    let refresh_token = text(tokens, "refresh_token")
        .map(str::to_string)
        .or_else(|| old.map(|credentials| credentials.refresh_token.clone()))
        .filter(|token| !token.is_empty())
        .ok_or("iFlow's sign-in sent no refresh token")?;
    let expires_in = tokens
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_EXPIRES_IN);
    Ok(Credentials {
        access_token: api_key.to_string(),
        refresh_token,
        expires_at: now_ms + expires_in * 1000,
        refreshed_at: now_ms,
        account_id: api_key.to_string(),
        email: if email.is_empty() {
            old.map(|credentials| credentials.email.clone())
                .unwrap_or_default()
        } else {
            email.to_string()
        },
        plan: old
            .map(|credentials| credentials.plan.clone())
            .unwrap_or_default(),
        project_id: old
            .map(|credentials| credentials.project_id.clone())
            .unwrap_or_default(),
    })
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn refused(status: u16, said: &str) -> String {
    if said.is_empty() {
        format!("iFlow's sign-in refused it (HTTP {status})")
    } else {
        format!("iFlow's sign-in refused it (HTTP {status}): {said}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::signin::take_pending;

    /// The real OAuth client comes from the server's environment; the tests
    /// use a fake one so they never need a credential.
    fn test_client() {
        static SET: std::sync::Once = std::sync::Once::new();
        SET.call_once(|| {
            std::env::set_var("RAST_IFLOW_CLIENT_ID", "test-iflow-id");
            std::env::set_var("RAST_IFLOW_CLIENT_SECRET", "test-iflow-secret");
        });
    }

    fn pending() -> Pending {
        Pending::browser(
            Plan::IFlow,
            7,
            None,
            "iFlow".into(),
            "the-state".into(),
            String::new(),
            REDIRECT_URI.into(),
        )
    }

    #[test]
    fn a_sign_in_starts_at_iflow_with_the_state_and_redirect() {
        test_client();
        let Started::Browser {
            url,
            state,
            redirect_uri,
        } = start(7, None, "iFlow".into(), 1_000).unwrap()
        else {
            panic!("iFlow signs in with a browser");
        };
        assert_eq!(redirect_uri, REDIRECT_URI);
        assert!(url.starts_with("https://iflow.cn/oauth?"));
        assert!(url.contains("client_id=test-iflow-id"));
        assert!(url.contains("loginMethod=phone"));
        assert!(url.contains("type=phone"));
        assert!(
            url.contains("redirect=http%3A%2F%2Flocalhost%3A56124%2Foauth2callback"),
            "{url}"
        );
        assert!(url.contains(&format!("state={state}")));
        // Only the account that started it, and only once.
        assert!(take_pending(&state, 8).is_err());
        let taken = take_pending(&state, 7).unwrap();
        assert_eq!(taken.plan, Plan::IFlow);
        assert_eq!(taken.redirect_uri, REDIRECT_URI);
        assert!(take_pending(&state, 7).is_err());
    }

    #[test]
    fn the_token_exchange_carries_the_code_redirect_and_secret() {
        let pending = pending();
        let pairs = token_pairs("test-id", "test-secret", &pending, "the-code");
        let field = |key: &str| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| *value)
        };
        assert_eq!(field("grant_type"), Some("authorization_code"));
        assert_eq!(field("code"), Some("the-code"));
        assert_eq!(field("redirect_uri"), Some(REDIRECT_URI));
        assert_eq!(field("client_id"), Some("test-id"));
        assert_eq!(field("client_secret"), Some("test-secret"));

        // The Basic header is the client id and secret joined by a colon.
        let basic = basic_auth("test-id", "test-secret");
        let encoded = basic.strip_prefix("Basic ").expect("Basic prefix");
        let decoded = STANDARD.decode(encoded).expect("base64");
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "test-id:test-secret".to_string()
        );
    }

    #[test]
    fn a_refresh_presents_the_refresh_token_and_secret() {
        let old = Credentials {
            access_token: "api-key".into(),
            refresh_token: "rt".into(),
            expires_at: 0,
            refreshed_at: 0,
            account_id: "api-key".into(),
            email: "someone@example.com".into(),
            plan: String::new(),
            project_id: String::new(),
        };
        let pairs = refresh_pairs("test-id", "test-secret", &old);
        assert!(pairs.contains(&("grant_type", "refresh_token")));
        assert!(pairs.contains(&("refresh_token", "rt")));
        assert!(pairs.contains(&("client_id", "test-id")));
        assert!(pairs.contains(&("client_secret", "test-secret")));
    }

    #[test]
    fn the_user_information_gives_the_api_key_and_an_email() {
        let value = serde_json::json!({
            "success": true,
            "data": {"apiKey": "the-api-key", "email": "someone@example.com", "nickname": "someone"}
        });
        assert_eq!(
            parse_user_info(&value),
            Ok(("the-api-key".to_string(), "someone@example.com".to_string()))
        );
        // Accounts registered by phone have no email: the phone names them.
        let value = serde_json::json!({
            "success": true,
            "data": {"apiKey": "k", "phone": "13800000000"}
        });
        assert_eq!(
            parse_user_info(&value),
            Ok(("k".to_string(), "13800000000".to_string()))
        );
        // The OAuth token is useless without the account's API key.
        let value = serde_json::json!({"success": true, "data": {"email": "e"}});
        assert!(parse_user_info(&value).unwrap_err().contains("no API key"));
        let value = serde_json::json!({"success": false, "message": "token expired"});
        assert_eq!(
            parse_user_info(&value),
            Err("iFlow did not say who signed in: token expired".into())
        );
        assert!(parse_user_info(&serde_json::json!({"success": false})).is_err());
    }

    #[test]
    fn the_api_key_is_the_stored_token_and_it_can_be_refreshed() {
        let tokens = serde_json::json!({
            "access_token": "oauth-token",
            "refresh_token": "rt",
            "expires_in": 7200
        });
        let first = credentials(&tokens, "api-key", "someone@example.com", None, 1_000)
            .expect("credentials");
        // The wire's bearer and the signing key are the API key, not the
        // OAuth access token.
        assert_eq!(first.access_token, "api-key");
        assert_eq!(first.account_id, "api-key");
        assert_eq!(first.refresh_token, "rt");
        assert_eq!(first.expires_at, 1_000 + 7_200_000);
        assert_eq!(first.email, "someone@example.com");

        // A refresh without a new refresh token or expiry keeps the old ones;
        // a reissued API key replaces the old one.
        let again = credentials(
            &serde_json::json!({"access_token": "oauth-2"}),
            "api-key-2",
            "",
            Some(&first),
            5_000,
        )
        .expect("credentials");
        assert_eq!(again.access_token, "api-key-2");
        assert_eq!(again.refresh_token, "rt");
        assert_eq!(again.email, "someone@example.com");
        assert_eq!(again.expires_at, 5_000 + 3_600_000);
        assert!(credentials(&serde_json::json!({}), "key", "e", None, 0).is_err());
    }

    #[test]
    fn the_user_information_url_carries_the_oauth_token() {
        let url = userinfo_url("a b+c");
        assert_eq!(
            url,
            "https://iflow.cn/api/oauth/getUserInfo?accessToken=a%20b%2Bc"
        );
    }
}
