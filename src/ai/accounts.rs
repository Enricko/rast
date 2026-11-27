//! One provider, several accounts (AI Agent settings → Providers).
//!
//! An account is a provider row whose `pool_id` names its provider; the
//! provider row is its own first account. Each reply takes one account by
//! the provider's [`Rotation`]: **fill first** uses the first account that
//! can answer and moves on when it reaches a limit; **round robin** takes
//! the accounts in turn (`sticky` replies each), so they run down together.
//! An account whose answer says it reached a limit rests until the reset the
//! provider gave (else [`REST_WITHOUT_RESET_MS`]) and the reply goes on with
//! the next account ([`super::run`]). Each answer's reported limits are kept
//! with the account for the quota page.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::{
    provider::limits::{Limit, Quota},
    provider_from_row, Provider,
};
use crate::{
    db::now_ms,
    error::{AppError, AppResult},
    secrets::Secrets,
};

/// How long an account rests after a limit the provider gave no reset for.
pub const REST_WITHOUT_RESET_MS: i64 = 60_000;
/// Replies in a row on one account, at most (round robin).
pub const MAX_STICKY: u32 = 100;
/// Accounts per provider, at most.
pub const MAX_ACCOUNTS: usize = 50;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The first account that can answer; the next when it reaches a limit.
    #[default]
    Fill,
    /// The accounts in turn.
    Round,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rotation {
    #[serde(default)]
    pub mode: Mode,
    /// Replies in a row on one account before the next takes its turn.
    #[serde(default = "one")]
    pub sticky: u32,
}

impl Default for Rotation {
    fn default() -> Self {
        Self {
            mode: Mode::Fill,
            sticky: 1,
        }
    }
}

fn one() -> u32 {
    1
}

/// The provider's accounts: the provider first, then the others in order.
pub async fn of(pool: &SqlitePool, provider: &Provider) -> AppResult<Vec<Provider>> {
    let first = provider.pool_id.as_deref().unwrap_or(&provider.id);
    let rows = sqlx::query(
        "SELECT * FROM ai_providers WHERE id = ? OR pool_id = ?
         ORDER BY pool_id IS NOT NULL, position, created_at, id",
    )
    .bind(first)
    .bind(first)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().filter_map(provider_from_row).collect())
}

/// Whether an account may answer now (not set aside, signed in or keyed,
/// not resting).
pub fn ready(account: &Provider, now: i64) -> bool {
    usable(account) && account.resting_until.is_none_or(|until| until <= now)
}

fn usable(account: &Provider) -> bool {
    !account.paused && account.has_credentials()
}

/// Whose turn it is per provider (round robin): the last account and how
/// many replies in a row it took.
static TURNS: LazyLock<Mutex<HashMap<String, (String, u32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The account for the next reply, among the accounts not in `tried`.
/// With `or_resting`, when every account rests the one back soonest tries
/// anyway (its provider then says how long the limit lasts).
pub fn choose(
    provider: &Provider,
    accounts: &[Provider],
    now: i64,
    tried: &[String],
    or_resting: bool,
) -> Option<Provider> {
    let open: Vec<&Provider> = accounts.iter().filter(|a| !tried.contains(&a.id)).collect();
    let ready: Vec<&Provider> = open.iter().copied().filter(|a| ready(a, now)).collect();
    if ready.is_empty() {
        if !or_resting {
            return None;
        }
        return open
            .into_iter()
            .filter(|a| usable(a))
            .min_by_key(|a| a.resting_until.unwrap_or(0))
            .cloned();
    }
    let chosen = match provider.rotation.mode {
        Mode::Fill => ready[0],
        Mode::Round => {
            let sticky = provider.rotation.sticky.clamp(1, MAX_STICKY);
            let mut turns = TURNS.lock().unwrap_or_else(|e| e.into_inner());
            let next = match turns.get(&provider.id) {
                Some((last, uses)) => match ready.iter().position(|a| &a.id == last) {
                    Some(i) if *uses < sticky => (i, uses + 1),
                    Some(i) => ((i + 1) % ready.len(), 1),
                    // The last one cannot answer now: the next after it.
                    None => {
                        let at = accounts.iter().position(|a| &a.id == last).unwrap_or(0);
                        let i = ready
                            .iter()
                            .position(|a| {
                                accounts.iter().position(|b| b.id == a.id).unwrap_or(0) > at
                            })
                            .unwrap_or(0);
                        (i, 1)
                    }
                },
                None => (0, 1),
            };
            turns.insert(provider.id.clone(), (ready[next.0].id.clone(), next.1));
            ready[next.0]
        }
    };
    Some(chosen.clone())
}

/// An account ready to answer, with its key or a fresh sign-in token.
pub struct Picked {
    pub account: Provider,
    pub api_key: Option<String>,
    /// The signed-in account id some plans send with each request.
    pub account_ref: String,
}

/// The account a reply on `provider` uses, with its credentials. An account
/// whose sign-in cannot be refreshed is skipped for this reply. `tried`
/// grows with each account taken. Without `or_resting`, only accounts that
/// are not resting (to go on after a limit).
pub async fn pick(
    pool: &SqlitePool,
    secrets: &Secrets,
    provider: &Provider,
    tried: &mut Vec<String>,
    or_resting: bool,
) -> AppResult<Option<Picked>> {
    let mut accounts = of(pool, provider).await?;
    // A plan's accounts are its owner's.
    if provider.kind.signs_in() {
        accounts.retain(|a| a.pool_id.is_none() || a.owner_id == provider.owner_id);
    }
    let mut failed: Option<AppError> = None;
    loop {
        let Some(account) = choose(provider, &accounts, now_ms(), tried, or_resting) else {
            if let Some(error) = failed {
                return Err(error);
            }
            if or_resting && tried.is_empty() {
                return Err(none_usable(provider, &accounts));
            }
            return Ok(None);
        };
        tried.push(account.id.clone());
        let credentials = if account.kind.signs_in() {
            super::signed_in_token(pool, secrets, &account)
                .await
                .map(|(token, account_ref)| (Some(token), account_ref))
        } else {
            account.api_key(secrets).map(|key| (key, String::new()))
        };
        match credentials {
            Ok((api_key, account_ref)) => {
                return Ok(Some(Picked {
                    account,
                    api_key,
                    account_ref,
                }))
            }
            Err(error) => {
                tracing::warn!(provider = %provider.name, account = %account.id, error = %error.public_message(), "AI account skipped");
                failed.get_or_insert(error);
            }
        }
    }
}

fn none_usable(provider: &Provider, accounts: &[Provider]) -> AppError {
    if accounts.iter().all(|a| a.paused) {
        return AppError::bad_request(format!(
            "every account of {} is set aside; an administrator turns one on in the AI Agent settings",
            provider.name
        ));
    }
    if provider.kind.signs_in() {
        AppError::bad_request(format!(
            "{} is not signed in; sign in again under AI Agent settings → Providers",
            provider.name
        ))
    } else {
        AppError::bad_request(format!(
            "{} has no API key yet; an administrator adds it in the AI Agent settings",
            provider.name
        ))
    }
}

/// When the next of `accounts` is back (ms), for messages.
pub fn back_soonest(accounts: &[Provider], now: i64) -> Option<i64> {
    accounts
        .iter()
        .filter(|a| usable(a))
        .filter_map(|a| a.resting_until.filter(|&t| t > now))
        .min()
}

/// Rest an account after a limit; returns until when.
pub async fn rest(pool: &SqlitePool, id: &str, limit: &Limit, now: i64) -> i64 {
    let until = limit
        .until
        .filter(|&t| t > now)
        .unwrap_or(now + REST_WITHOUT_RESET_MS);
    let stored =
        sqlx::query("UPDATE ai_providers SET resting_until = ?, resting_reason = ? WHERE id = ?")
            .bind(until)
            .bind(&limit.reason)
            .bind(id)
            .execute(pool)
            .await;
    if let Err(e) = stored {
        tracing::warn!(error = %e, "could not mark an AI account as resting");
    }
    until
}

/// Let a resting account answer again now.
pub async fn wake(pool: &SqlitePool, id: &str) -> AppResult<()> {
    sqlx::query("UPDATE ai_providers SET resting_until = NULL, resting_reason = '' WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Keep the limits an answer reported for the account.
pub async fn store_quota(pool: &SqlitePool, id: &str, quota: &Quota) {
    if quota.is_empty() {
        return;
    }
    let text = serde_json::to_string(quota).unwrap_or_else(|_| "{}".into());
    let stored = sqlx::query("UPDATE ai_providers SET quota = ? WHERE id = ?")
        .bind(text)
        .bind(id)
        .execute(pool)
        .await;
    if let Err(e) = stored {
        tracing::warn!(error = %e, "could not store an AI account's limits");
    }
}

/// "in 2 h 10 min", "in 40 s" — for messages.
pub fn in_words(ms: i64) -> String {
    let secs = (ms / 1000).max(1);
    if secs < 60 {
        format!("in {secs} s")
    } else if secs < 3600 {
        format!("in {} min", (secs + 59) / 60)
    } else if secs < 86_400 {
        let (h, m) = (secs / 3600, (secs % 3600) / 60);
        if m == 0 {
            format!("in {h} h")
        } else {
            format!("in {h} h {m} min")
        }
    } else {
        let (d, h) = (secs / 86_400, (secs % 86_400) / 3600);
        if h == 0 {
            format!("in {d} d")
        } else {
            format!("in {d} d {h} h")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, pool_id: Option<&str>) -> Provider {
        Provider {
            id: id.into(),
            kind: super::super::Kind::Custom,
            name: id.into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: String::new(),
            models: vec![],
            prices: Default::default(),
            list_prices: Default::default(),
            prices_checked_at: None,
            enabled: true,
            created_at: 0,
            updated_at: 0,
            owner_id: None,
            sign_in: String::new(),
            pool_id: pool_id.map(str::to_string),
            position: 0,
            rotation: Rotation::default(),
            resting_until: None,
            resting_reason: String::new(),
            quota: Quota::default(),
            paused: false,
        }
    }

    fn ids(picks: impl Iterator<Item = Option<Provider>>) -> Vec<String> {
        picks.map(|p| p.map(|p| p.id).unwrap_or_default()).collect()
    }

    #[test]
    fn fill_first_moves_on_when_an_account_rests() {
        let provider = account("p", None);
        let mut accounts = vec![
            provider.clone(),
            account("b", Some("p")),
            account("c", Some("p")),
        ];
        assert_eq!(
            choose(&provider, &accounts, 10, &[], false).unwrap().id,
            "p"
        );
        accounts[0].resting_until = Some(100);
        assert_eq!(
            choose(&provider, &accounts, 10, &[], false).unwrap().id,
            "b"
        );
        // Back after its rest.
        assert_eq!(
            choose(&provider, &accounts, 100, &[], false).unwrap().id,
            "p"
        );
        // Set aside, or already tried in this reply.
        accounts[1].paused = true;
        assert_eq!(
            choose(&provider, &accounts, 10, &[], false).unwrap().id,
            "c"
        );
        assert!(choose(&provider, &accounts, 10, &["c".into()], false).is_none());
        // Every account rests: the one back soonest tries anyway.
        accounts[2].resting_until = Some(50);
        assert!(choose(&provider, &accounts, 10, &[], false).is_none());
        assert_eq!(choose(&provider, &accounts, 10, &[], true).unwrap().id, "c");
        assert_eq!(back_soonest(&accounts, 10), Some(50));
    }

    #[test]
    fn round_robin_takes_turns_and_skips_resting_accounts() {
        let mut provider = account("rr", None);
        provider.rotation = Rotation {
            mode: Mode::Round,
            sticky: 1,
        };
        let mut accounts = vec![
            provider.clone(),
            account("rr-b", Some("rr")),
            account("rr-c", Some("rr")),
        ];
        let turns = ids((0..4).map(|_| choose(&provider, &accounts, 10, &[], false)));
        assert_eq!(turns, ["rr", "rr-b", "rr-c", "rr"]);
        accounts[1].resting_until = Some(100);
        let turns = ids((0..3).map(|_| choose(&provider, &accounts, 10, &[], false)));
        assert_eq!(turns, ["rr-c", "rr", "rr-c"]);

        let mut sticky = account("st", None);
        sticky.rotation = Rotation {
            mode: Mode::Round,
            sticky: 2,
        };
        let accounts = vec![sticky.clone(), account("st-b", Some("st"))];
        let turns = ids((0..5).map(|_| choose(&sticky, &accounts, 10, &[], false)));
        assert_eq!(turns, ["st", "st", "st-b", "st-b", "st"]);
    }

    #[test]
    fn rests_read_as_words() {
        assert_eq!(in_words(40_000), "in 40 s");
        assert_eq!(in_words(600_000), "in 10 min");
        assert_eq!(in_words(7_800_000), "in 2 h 10 min");
        assert_eq!(in_words(2 * 86_400_000), "in 2 d");
    }
}
