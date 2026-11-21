//! The account API end to end: setup, sign-in, roles and sessions.

mod common;

use axum::http::StatusCode;
use common::{Client, TOKEN};
use serde_json::json;

#[tokio::test]
async fn setup_needs_the_token_and_happens_once() {
    let client = Client::new().await;
    let state = client.call("GET", "/api/auth/state", None, None).await;
    assert_eq!(state.body["needsSetup"], true);
    assert!(state.body["user"].is_null());

    let wrong = client
        .call(
            "POST",
            "/api/auth/setup",
            None,
            Some(json!({"username": "root", "password": "root password", "token": "nope"})),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);

    let session = client.setup().await;
    let state = client
        .call("GET", "/api/auth/state", Some(&session), None)
        .await;
    assert_eq!(state.body["needsSetup"], false);
    assert_eq!(state.body["user"]["username"], "root");
    assert_eq!(state.body["user"]["role"], "superadmin");

    let again = client
        .call(
            "POST",
            "/api/auth/setup",
            None,
            Some(json!({"username": "other", "password": "other password", "token": TOKEN})),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn sign_in_works_and_wrong_passwords_are_throttled() {
    let client = Client::new().await;
    client.setup().await;
    let ok = client.login("ROOT", "root password").await;
    assert_eq!(ok.status, StatusCode::OK, "usernames are case-insensitive");
    assert!(ok.cookie.is_some());

    let unknown = client.login("ghost", "whatever pass").await;
    let wrong = client.login("root", "wrong password").await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown.body, wrong.body, "no hint which part was wrong");

    for _ in 0..4 {
        client.login("root", "wrong password").await;
    }
    let locked = client.login("root", "root password").await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn changes_need_the_rast_header() {
    let client = Client::new().await;
    let reply = client
        .call_with(
            "POST",
            "/api/auth/login",
            None,
            Some(json!({"username": "root", "password": "root password"})),
            false,
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn admins_manage_users_but_only_super_admins_manage_admins() {
    let client = Client::new().await;
    let root = client.setup().await;
    assert_eq!(
        client.create(&root, "ada", "admin").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        client.create(&root, "bob", "user").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        client.create(&root, "bob", "user").await.status,
        StatusCode::CONFLICT
    );

    let ada = client.login("ada", "a good password").await.cookie.unwrap();
    let bob = client.login("bob", "a good password").await.cookie.unwrap();

    // A user can't see or change accounts.
    assert_eq!(
        client
            .call("GET", "/api/users", Some(&bob), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client.create(&bob, "eve", "user").await.status,
        StatusCode::FORBIDDEN
    );

    // An admin creates users, not admins.
    assert_eq!(
        client.create(&ada, "cat", "user").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        client.create(&ada, "dan", "admin").await.status,
        StatusCode::FORBIDDEN
    );

    let list = client.call("GET", "/api/users", Some(&ada), None).await;
    let names: Vec<&str> = list.body["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["root", "ada", "bob", "cat"], "Super Admins first");
    let id = |name: &str| {
        list.body["users"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["username"] == name)
            .unwrap()["id"]
            .as_i64()
            .unwrap()
    };

    // Admins can't promote, or touch the Super Admin.
    let promote = client
        .call(
            "PATCH",
            &format!("/api/users/{}", id("cat")),
            Some(&ada),
            Some(json!({"role": "admin"})),
        )
        .await;
    assert_eq!(promote.status, StatusCode::FORBIDDEN);
    let touch_root = client
        .call(
            "PATCH",
            &format!("/api/users/{}", id("root")),
            Some(&ada),
            Some(json!({"disabled": true})),
        )
        .await;
    assert_eq!(touch_root.status, StatusCode::FORBIDDEN);

    // Disabling ends the user's session and blocks sign-in.
    let disable = client
        .call(
            "PATCH",
            &format!("/api/users/{}", id("bob")),
            Some(&ada),
            Some(json!({"disabled": true})),
        )
        .await;
    assert_eq!(disable.status, StatusCode::OK);
    assert_eq!(disable.body["user"]["disabled"], true);
    let state = client
        .call("GET", "/api/auth/state", Some(&bob), None)
        .await;
    assert!(state.body["user"].is_null(), "the session ended");
    assert_eq!(
        client.login("bob", "a good password").await.status,
        StatusCode::FORBIDDEN
    );

    // The Super Admin promotes and deletes.
    let promote = client
        .call(
            "PATCH",
            &format!("/api/users/{}", id("cat")),
            Some(&root),
            Some(json!({"role": "admin"})),
        )
        .await;
    assert_eq!(promote.body["user"]["role"], "admin");
    let delete = client
        .call(
            "DELETE",
            &format!("/api/users/{}", id("bob")),
            Some(&root),
            None,
        )
        .await;
    assert_eq!(delete.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn the_last_super_admin_stays_and_nobody_edits_themselves() {
    let client = Client::new().await;
    let root = client.setup().await;
    client.create(&root, "sam", "superadmin").await;
    let sam = client.login("sam", "a good password").await.cookie.unwrap();
    let users = client
        .call("GET", "/api/users", Some(&root), None)
        .await
        .body;
    let root_id = users["users"][0]["id"].as_i64().unwrap();
    let sam_id = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "sam")
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    let self_demote = client
        .call(
            "PATCH",
            &format!("/api/users/{root_id}"),
            Some(&root),
            Some(json!({"role": "user"})),
        )
        .await;
    assert_eq!(self_demote.status, StatusCode::FORBIDDEN);
    let self_delete = client
        .call(
            "DELETE",
            &format!("/api/users/{root_id}"),
            Some(&root),
            None,
        )
        .await;
    assert_eq!(self_delete.status, StatusCode::FORBIDDEN);

    // With two Super Admins, one may demote the other, but not the last one.
    let demote = client
        .call(
            "PATCH",
            &format!("/api/users/{sam_id}"),
            Some(&root),
            Some(json!({"role": "admin"})),
        )
        .await;
    assert_eq!(demote.status, StatusCode::OK);
    client
        .call(
            "PATCH",
            &format!("/api/users/{sam_id}"),
            Some(&root),
            Some(json!({"role": "superadmin"})),
        )
        .await;
    client
        .call(
            "PATCH",
            &format!("/api/users/{root_id}"),
            Some(&sam),
            Some(json!({"disabled": true})),
        )
        .await;
    let last = client
        .call(
            "PATCH",
            &format!("/api/users/{root_id}"),
            Some(&sam),
            Some(json!({"role": "user"})),
        )
        .await;
    assert_eq!(
        last.status,
        StatusCode::OK,
        "root was disabled, sam remains"
    );
    let lone = client
        .call("DELETE", &format!("/api/users/{sam_id}"), Some(&sam), None)
        .await;
    assert_eq!(lone.status, StatusCode::FORBIDDEN, "not yourself");
}

#[tokio::test]
async fn changing_your_password_ends_your_other_sessions() {
    let client = Client::new().await;
    let first = client.setup().await;
    let second = client.login("root", "root password").await.cookie.unwrap();
    let wrong = client
        .call(
            "POST",
            "/api/auth/password",
            Some(&first),
            Some(json!({"current": "not it at all", "new": "a newer password"})),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    let ok = client
        .call(
            "POST",
            "/api/auth/password",
            Some(&first),
            Some(json!({"current": "root password", "new": "a newer password"})),
        )
        .await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT);
    let kept = client
        .call("GET", "/api/auth/state", Some(&first), None)
        .await;
    assert_eq!(kept.body["user"]["username"], "root");
    let ended = client
        .call("GET", "/api/auth/state", Some(&second), None)
        .await;
    assert!(ended.body["user"].is_null());

    let logout = client
        .call("POST", "/api/auth/logout", Some(&first), None)
        .await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    let gone = client
        .call("GET", "/api/auth/state", Some(&first), None)
        .await;
    assert!(gone.body["user"].is_null());
    assert_eq!(
        client.login("root", "a newer password").await.status,
        StatusCode::OK
    );
}
