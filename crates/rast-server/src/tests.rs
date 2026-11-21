//! Sign-in protection that needs the server's insides. The in-memory throttle
//! answers "wait" from the fifth wrong password on, so a test cannot fail ten
//! times in a row quickly; it records the failures directly instead.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{AppState, db, router};

const TOKEN: &str = "internal-test-token-1234";

struct Reply {
    status: StatusCode,
    body: Value,
    cookie: Option<String>,
}

async fn call(app: &Router, method: &str, path: &str, session: Option<&str>, body: Value) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("x-rast", "1")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(session) = session {
        request = request.header(header::COOKIE, format!("rast_session={session}"));
    }
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(body.to_string())
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("rast_session="))
        .and_then(|v| v.split(';').next())
        .map(str::to_owned);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        cookie,
    }
}

#[tokio::test]
async fn a_locked_account_waits_until_an_administrator_unlocks_it() {
    let state = AppState::in_memory(TOKEN).await.unwrap();
    let app = router(state.clone());
    let root = call(
        &app,
        "POST",
        "/api/auth/setup",
        None,
        json!({"username": "root", "password": "root password", "token": TOKEN}),
    )
    .await
    .cookie
    .unwrap();
    let bob = call(
        &app,
        "POST",
        "/api/users",
        Some(&root),
        json!({"username": "bob", "password": "a good password", "role": "user"}),
    )
    .await;
    let bob_id = bob.body["user"]["id"].as_i64().unwrap();
    let login = json!({"username": "bob", "password": "a good password"});

    for _ in 0..db::LOCK_AFTER {
        db::record_failure(&state.pool, bob_id).await.unwrap();
    }
    let locked = call(&app, "POST", "/api/auth/login", None, login.clone()).await;
    assert_eq!(
        locked.status,
        StatusCode::TOO_MANY_REQUESTS,
        "even the right password waits"
    );
    assert_eq!(
        locked.body["error"],
        "Too many attempts. Try again in 15 minutes."
    );
    let list = call(&app, "GET", "/api/users", Some(&root), Value::Null).await;
    let row = list.body["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "bob")
        .unwrap()
        .clone();
    assert!(row["lockedUntil"].as_i64().unwrap() > db::now());

    let unlocked = call(
        &app,
        "PATCH",
        &format!("/api/users/{bob_id}"),
        Some(&root),
        json!({"unlock": true}),
    )
    .await;
    assert_eq!(unlocked.status, StatusCode::OK, "{}", unlocked.body);
    assert_eq!(unlocked.body["user"]["lockedUntil"], Value::Null);
    let login = call(&app, "POST", "/api/auth/login", None, login).await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.body);
}

#[tokio::test]
async fn wrong_passwords_count_towards_the_lockout() {
    let state = AppState::in_memory(TOKEN).await.unwrap();
    let app = router(state.clone());
    call(
        &app,
        "POST",
        "/api/auth/setup",
        None,
        json!({"username": "root", "password": "root password", "token": TOKEN}),
    )
    .await;
    for _ in 0..3 {
        let wrong = call(
            &app,
            "POST",
            "/api/auth/login",
            None,
            json!({"username": "root", "password": "not the password"}),
        )
        .await;
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    }
    let root = db::user_by_name(&state.pool, "root")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root.recent_failures(db::now()), 3);
    call(
        &app,
        "POST",
        "/api/auth/login",
        None,
        json!({"username": "root", "password": "root password"}),
    )
    .await;
    let root = db::user_by_name(&state.pool, "root")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root.recent_failures(db::now()), 0);
}

#[tokio::test]
async fn a_locked_account_is_throttled_even_after_a_restart() {
    // A fresh state stands for a restart: the lock is in the database, the
    // in-memory throttle is empty.
    let state = AppState::in_memory(TOKEN).await.unwrap();
    let app = router(state.clone());
    let root = call(
        &app,
        "POST",
        "/api/auth/setup",
        None,
        json!({"username": "root", "password": "root password", "token": TOKEN}),
    )
    .await
    .cookie
    .unwrap();
    let bob = call(
        &app,
        "POST",
        "/api/users",
        Some(&root),
        json!({"username": "bob", "password": "a good password", "role": "user"}),
    )
    .await;
    let bob_id = bob.body["user"]["id"].as_i64().unwrap();
    for _ in 0..db::LOCK_AFTER {
        db::record_failure(&state.pool, bob_id).await.unwrap();
    }

    let login = json!({"username": "bob", "password": "a good password"});
    for _ in 0..8 {
        let reply = call(&app, "POST", "/api/auth/login", None, login.clone()).await;
        assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    }
    let checked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'login.locked'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
    assert_eq!(
        checked, 5,
        "after five tries the throttle answers before any password check"
    );
}
