//! A client for the API router, shared by the integration tests.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

pub const TOKEN: &str = "test-setup-token-123456";

pub struct Client {
    app: Router,
}

pub struct Reply {
    pub status: StatusCode,
    pub body: Value,
    pub cookie: Option<String>,
}

impl Client {
    pub async fn new() -> Self {
        let state = rast_server::AppState::in_memory(TOKEN).await.unwrap();
        Self {
            app: rast_server::router(state),
        }
    }

    pub async fn call(
        &self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        self.call_with(method, path, session, body, true).await
    }

    pub async fn call_with(
        &self,
        method: &str,
        path: &str,
        session: Option<&str>,
        body: Option<Value>,
        marked: bool,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(path);
        if marked {
            request = request.header("x-rast", "1");
        }
        if let Some(session) = session {
            request = request.header(header::COOKIE, format!("rast_session={session}"));
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("rast_session="))
            .and_then(|v| v.split(';').next())
            .map(str::to_owned);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Reply {
            status,
            body,
            cookie,
        }
    }

    /// Set up with a Super Admin `root` and return its session.
    pub async fn setup(&self) -> String {
        let reply = self
            .call(
                "POST",
                "/api/auth/setup",
                None,
                Some(json!({"username": "root", "password": "root password", "token": TOKEN})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        reply.cookie.expect("setup signs in")
    }

    pub async fn create(&self, session: &str, username: &str, role: &str) -> Reply {
        self.call(
            "POST",
            "/api/users",
            Some(session),
            Some(json!({"username": username, "password": "a good password", "role": role})),
        )
        .await
    }

    pub async fn login(&self, username: &str, password: &str) -> Reply {
        self.call(
            "POST",
            "/api/auth/login",
            None,
            Some(json!({"username": username, "password": password})),
        )
        .await
    }
}
