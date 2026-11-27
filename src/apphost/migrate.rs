//! Handing AI Agent's data to its Package Center app, once: dump and
//! restore, never a copy of database files. The app (app id `agent`) reads
//! the rows of RAST's `ai_*` tables and its two settings
//! (`GET /v1/migrate/ai`), asks RAST to decrypt the values RAST encrypted
//! (`POST /v1/migrate/ai/decrypt`; the app encrypts them again with its own
//! key), copies the chat files from `filesDir`, and says when it is done
//! (`POST /v1/migrate/ai/done`). RAST's own tables stay as they are, as a
//! backup, until an administrator removes them.

use std::sync::Arc;

use axum::{extract::State, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use super::App;
use crate::{
    db,
    error::{AppError, AppResult},
    state::AppState,
};

/// RAST's AI Agent tables, parents first.
pub const AI_TABLES: [&str; 8] = [
    "ai_providers",
    "ai_skills",
    "ai_agents",
    "ai_limits",
    "ai_conversations",
    "ai_messages",
    "ai_usage",
    "ai_files",
];
const AI_SETTINGS: [&str; 2] = ["ai.search", "ai.serverAccess"];
/// Set once the app has taken the data (its time, Unix ms).
pub const HANDED_OVER: &str = "apps.agent.handedOverAt";

fn only_agent(app: &App) -> AppResult<()> {
    if app.app_id == "agent" {
        Ok(())
    } else {
        Err(AppError::Forbidden("only AI Agent takes this data"))
    }
}

async fn handed_over(state: &AppState) -> Option<String> {
    db::get_setting(&state.db, HANDED_OVER)
        .await
        .filter(|v| !v.is_empty())
}

/// Every row of `table` as JSON objects (column name → value).
async fn rows(state: &AppState, table: &str) -> AppResult<Option<Vec<Value>>> {
    let exists: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_optional(&state.db)
            .await?;
    if exists.is_none() {
        return Ok(None);
    }
    let columns: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
        .bind(table)
        .fetch_all(&state.db)
        .await?;
    let pairs: Vec<String> = columns
        .iter()
        .map(|c| {
            format!(
                "'{}', \"{}\"",
                c.replace('\'', "''"),
                c.replace('"', "\"\"")
            )
        })
        .collect();
    // Table names are the fixed list above; column names come from SQLite.
    let sql = format!("SELECT json_object({}) FROM \"{table}\"", pairs.join(", "));
    let texts: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_all(&state.db)
        .await?;
    let values = texts
        .iter()
        .map(|t| serde_json::from_str(t).unwrap_or(Value::Null))
        .collect();
    Ok(Some(values))
}

/// GET /v1/migrate/ai
pub async fn dump(
    State(state): State<AppState>,
    Extension(app): Extension<Arc<App>>,
) -> AppResult<Json<Value>> {
    only_agent(&app)?;
    if let Some(at) = handed_over(&state).await {
        return Ok(Json(json!({ "available": false, "handedOverAt": at })));
    }
    let mut tables = serde_json::Map::new();
    for table in AI_TABLES {
        if let Some(list) = rows(&state, table).await? {
            tables.insert(table.to_string(), Value::Array(list));
        }
    }
    let mut settings = serde_json::Map::new();
    for key in AI_SETTINGS {
        if let Some(value) = db::get_setting(&state.db, key).await {
            settings.insert(key.to_string(), Value::String(value));
        }
    }
    Ok(Json(json!({
        "available": true,
        "tables": tables,
        "settings": settings,
        "filesDir": state.config.data_dir.join("ai-files"),
    })))
}

#[derive(Deserialize)]
pub struct Encrypted {
    values: Vec<String>,
}

/// POST /v1/migrate/ai/decrypt — `{values}` → `{values}` (null where a
/// value cannot be read).
pub async fn decrypt(
    State(state): State<AppState>,
    Extension(app): Extension<Arc<App>>,
    Json(body): Json<Encrypted>,
) -> AppResult<Json<Value>> {
    only_agent(&app)?;
    if handed_over(&state).await.is_some() {
        return Err(AppError::conflict(
            "AI Agent's data was handed over already",
        ));
    }
    let values: Vec<Value> = body
        .values
        .iter()
        .map(|v| {
            state
                .secrets
                .decrypt_string(v)
                .map(Value::String)
                .unwrap_or(Value::Null)
        })
        .collect();
    Ok(Json(json!({ "values": values })))
}

/// POST /v1/migrate/ai/done
pub async fn done(
    State(state): State<AppState>,
    Extension(app): Extension<Arc<App>>,
) -> AppResult<Json<Value>> {
    only_agent(&app)?;
    let at = db::now_ms().to_string();
    db::set_setting(&state.db, HANDED_OVER, &at).await?;
    tracing::info!("AI Agent's data was handed to its Package Center app");
    Ok(Json(json!({ "handedOverAt": at })))
}
