//! Activity log endpoints with pagination, filters and CSV export.

use axum::{
    extract::{Query, State},
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqliteArguments, Row, Sqlite};
use sqlx::query::Query as SqlQuery;

use crate::{
    auth::AuthUser,
    error::AppResult,
    state::AppState,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/logs", get(list_logs))
        .route("/api/logs/actions", get(list_actions))
        .route("/api/logs/export", get(export_logs))
}

#[derive(Debug, Deserialize)]
pub struct LogQuery {
    pub page: Option<i64>,
    #[serde(rename = "perPage")]
    pub per_page: Option<i64>,
    pub actor: Option<String>,
    pub action: Option<String>,
    pub result: Option<String>,
    #[serde(rename = "from")]
    pub from_ts: Option<i64>,
    #[serde(rename = "to")]
    pub to_ts: Option<i64>,
    pub q: Option<String>,
    #[serde(rename = "userId")]
    pub user_id: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub id: i64,
    pub created_at: i64,
    pub user_id: Option<i64>,
    pub username: String,
    pub action: String,
    pub object_type: String,
    pub object_id: String,
    pub object_path: String,
    pub result: String,
    pub detail: String,
    pub ip: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogPage {
    pub items: Vec<LogEntry>,
    pub page: i64,
    pub per_page: i64,
    pub total: i64,
}

/// Build the dynamic WHERE clause. Only static fragments are interpolated;
/// every user value is bound as a parameter.
fn where_clause(user: &AuthUser, q: &LogQuery) -> String {
    let mut clauses: Vec<String> = Vec::new();
    if !user.is_admin() || q.user_id.is_some() {
        clauses.push("user_id = ?".to_string());
    }
    if q.actor.as_deref().is_some_and(|a| !a.trim().is_empty()) {
        clauses.push("username LIKE ?".to_string());
    }
    if q.action.as_deref().is_some_and(|a| !a.trim().is_empty()) {
        clauses.push("action = ?".to_string());
    }
    if q.result.as_deref().is_some_and(|r| !r.trim().is_empty()) {
        clauses.push("result = ?".to_string());
    }
    if q.q.as_deref().is_some_and(|s| !s.trim().is_empty()) {
        clauses.push("(object_path LIKE ? OR detail LIKE ?)".to_string());
    }
    if q.from_ts.is_some() {
        clauses.push("created_at >= ?".to_string());
    }
    if q.to_ts.is_some() {
        clauses.push("created_at <= ?".to_string());
    }
    if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    }
}

fn bind_filters<'q>(
    mut query: SqlQuery<'q, Sqlite, SqliteArguments>,
    user: &'q AuthUser,
    q: &LogQuery,
) -> SqlQuery<'q, Sqlite, SqliteArguments> {
    if !user.is_admin() {
        query = query.bind(user.id);
    } else if let Some(uid) = q.user_id {
        query = query.bind(uid);
    }
    if let Some(actor) = q.actor.as_deref().filter(|a| !a.trim().is_empty()) {
        query = query.bind(format!("%{}%", actor.trim()));
    }
    if let Some(action) = q.action.as_deref().filter(|a| !a.trim().is_empty()) {
        query = query.bind(action.trim().to_string());
    }
    if let Some(result) = q.result.as_deref().filter(|r| !r.trim().is_empty()) {
        query = query.bind(result.trim().to_string());
    }
    if let Some(search) = q.q.as_deref().filter(|s| !s.trim().is_empty()) {
        let like = format!("%{}%", search.trim());
        query = query.bind(like.clone()).bind(like);
    }
    if let Some(from) = q.from_ts {
        query = query.bind(from);
    }
    if let Some(to) = q.to_ts {
        query = query.bind(to);
    }
    query
}

/// GET /api/logs
pub async fn list_logs(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<LogQuery>,
) -> AppResult<Json<LogPage>> {
    let page = q.page.unwrap_or(1).max(1);
    let per_page = q.per_page.unwrap_or(50).clamp(1, 200);
    let offset = (page - 1) * per_page;
    let where_sql = where_clause(&user, &q);

    let count_sql = format!("SELECT COUNT(*) AS n FROM audit_logs{where_sql}");
    let total: i64 = bind_filters(sqlx::query(sqlx::AssertSqlSafe(count_sql)), &user, &q)
        .fetch_one(&state.db)
        .await?
        .get("n");

    let rows_sql = format!(
        "SELECT id, created_at, user_id, username, action, object_type, object_id,
                object_path, result, detail, ip
         FROM audit_logs{where_sql} ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?"
    );
    let rows = bind_filters(sqlx::query(sqlx::AssertSqlSafe(rows_sql)), &user, &q)
        .bind(per_page)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
    let items = rows
        .into_iter()
        .map(|r| LogEntry {
            id: r.get("id"),
            created_at: r.get("created_at"),
            user_id: r.get("user_id"),
            username: r.get("username"),
            action: r.get("action"),
            object_type: r.get("object_type"),
            object_id: r.get("object_id"),
            object_path: r.get("object_path"),
            result: r.get("result"),
            detail: r.get("detail"),
            ip: r.get("ip"),
        })
        .collect();
    Ok(Json(LogPage {
        items,
        page,
        per_page,
        total,
    }))
}

/// GET /api/logs/actions — distinct action values for the filter UI.
pub async fn list_actions(
    State(state): State<AppState>,
    user: AuthUser,
) -> AppResult<Json<serde_json::Value>> {
    let rows: Vec<String> = if user.is_admin() {
        sqlx::query_scalar("SELECT DISTINCT action FROM audit_logs ORDER BY action LIMIT 200")
            .fetch_all(&state.db)
            .await?
    } else {
        sqlx::query_scalar(
            "SELECT DISTINCT action FROM audit_logs WHERE user_id = ? ORDER BY action LIMIT 200",
        )
        .bind(user.id)
        .fetch_all(&state.db)
        .await?
    };
    Ok(Json(serde_json::json!({ "items": rows })))
}

/// GET /api/logs/export — CSV of up to 10k matching rows.
pub async fn export_logs(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<LogQuery>,
) -> AppResult<Response> {
    let where_sql = where_clause(&user, &q);
    let sql = format!(
        "SELECT created_at, username, action, object_type, object_path, result, detail, ip
         FROM audit_logs{where_sql} ORDER BY created_at DESC LIMIT 10000"
    );
    let rows = bind_filters(sqlx::query(sqlx::AssertSqlSafe(sql)), &user, &q)
        .fetch_all(&state.db)
        .await?;
    let mut out = String::from("timestamp,user,action,object_type,object_path,result,detail,ip\n");
    for r in rows {
        let ts: i64 = r.get("created_at");
        let fields = [
            ts.to_string(),
            r.get::<String, _>("username"),
            r.get::<String, _>("action"),
            r.get::<String, _>("object_type"),
            r.get::<String, _>("object_path"),
            r.get::<String, _>("result"),
            r.get::<String, _>("detail"),
            r.get::<String, _>("ip"),
        ];
        out.push_str(&fields.iter().map(|f| csv_escape(f)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    let mut response = out.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"rast-activity-logs.csv\""),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

