//! Account protection end to end: passwords an administrator set, sessions on
//! several devices, and people's own display names.

mod common;

use axum::http::StatusCode;
use common::Client;
use serde_json::{Value, json};

const CHROME_WINDOWS: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36";
const FIREFOX_LINUX: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";

async fn sessions(client: &Client, session: &str, path: &str) -> Vec<Value> {
    let reply = client.call("GET", path, Some(session), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["sessions"].as_array().unwrap().clone()
}

async fn signed_in(client: &Client, session: &str) -> bool {
    !client
        .call("GET", "/api/auth/state", Some(session), None)
        .await
        .body["user"]
        .is_null()
}

#[tokio::test]
async fn a_password_an_administrator_set_must_be_replaced_first() {
    let client = Client::new().await;
    let root = client.setup().await;
    let created = client
        .call(
            "POST",
            "/api/users",
            Some(&root),
            Some(
                json!({"username": "bob", "password": "temporary pass", "role": "user",
                        "mustChangePassword": true}),
            ),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["user"]["mustChangePassword"], true);
    let bob_id = created.body["user"]["id"].as_i64().unwrap();

    let login = client.login("bob", "temporary pass").await;
    assert_eq!(login.status, StatusCode::OK);
    assert_eq!(login.body["user"]["mustChangePassword"], true);
    let bob = login.cookie.unwrap();
    let blocked = client.call("GET", "/api/access/me", Some(&bob), None).await;
    assert_eq!(blocked.status, StatusCode::FORBIDDEN);
    assert!(
        blocked.body["error"]
            .as_str()
            .unwrap()
            .contains("new password")
    );

    let same = client
        .call(
            "POST",
            "/api/auth/password",
            Some(&bob),
            Some(json!({"current": "temporary pass", "new": "temporary pass"})),
        )
        .await;
    assert_eq!(same.status, StatusCode::BAD_REQUEST, "a new one, please");
    let chosen = client
        .call(
            "POST",
            "/api/auth/password",
            Some(&bob),
            Some(json!({"current": "temporary pass", "new": "bob's own secret"})),
        )
        .await;
    assert_eq!(chosen.status, StatusCode::NO_CONTENT);
    let state = client
        .call("GET", "/api/auth/state", Some(&bob), None)
        .await;
    assert_eq!(state.body["user"]["mustChangePassword"], false);
    assert_eq!(
        client
            .call("GET", "/api/access/me", Some(&bob), None)
            .await
            .status,
        StatusCode::OK
    );

    // A reset by an administrator signs bob out and asks again, unless the
    // administrator says otherwise.
    let reset = client
        .call(
            "PATCH",
            &format!("/api/users/{bob_id}"),
            Some(&root),
            Some(json!({"password": "another temporary"})),
        )
        .await;
    assert_eq!(reset.body["user"]["mustChangePassword"], true);
    assert!(
        !signed_in(&client, &bob).await,
        "the reset ended the session"
    );
    let keep = client
        .call(
            "PATCH",
            &format!("/api/users/{bob_id}"),
            Some(&root),
            Some(json!({"password": "a lasting password", "mustChangePassword": false})),
        )
        .await;
    assert_eq!(keep.body["user"]["mustChangePassword"], false);
    let login = client.login("bob", "a lasting password").await;
    assert_eq!(login.body["user"]["mustChangePassword"], false);

    // Your own password changes under Profile, with the current one.
    let root_id = client.id_of(&root, "root").await;
    let own = client
        .call(
            "PATCH",
            &format!("/api/users/{root_id}"),
            Some(&root),
            Some(json!({"password": "no current needed?"})),
        )
        .await;
    assert_eq!(own.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn people_see_and_end_their_sessions() {
    let client = Client::new().await;
    let setup = client.setup().await;
    let login = |agent: &'static str, ip: &'static str| {
        let client = &client;
        async move {
            client
                .call_from(
                    "POST",
                    "/api/auth/login",
                    None,
                    Some(json!({"username": "root", "password": "root password"})),
                    &[("user-agent", agent), ("x-client-ip", ip)],
                )
                .await
                .cookie
                .unwrap()
        }
    };
    let laptop = login(CHROME_WINDOWS, "203.0.113.5").await;
    let desktop = login(FIREFOX_LINUX, "198.51.100.9").await;

    let list = sessions(&client, &laptop, "/api/auth/sessions").await;
    assert_eq!(list.len(), 3, "setup, laptop and desktop");
    let current: Vec<&Value> = list.iter().filter(|s| s["current"] == true).collect();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["device"], "Chrome on Windows");
    assert_eq!(current[0]["ip"], "203.0.113.5");
    let desktop_id = list
        .iter()
        .find(|s| s["device"] == "Firefox on Linux")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(desktop_id.len(), 32);
    assert!(
        list.iter().all(|s| s.get("tokenHash").is_none()),
        "token hashes stay on the server"
    );

    let ended = client
        .call(
            "DELETE",
            &format!("/api/auth/sessions/{desktop_id}"),
            Some(&laptop),
            None,
        )
        .await;
    assert_eq!(ended.status, StatusCode::OK);
    assert_eq!(ended.body["sessions"].as_array().unwrap().len(), 2);
    assert!(!signed_in(&client, &desktop).await);
    let again = client
        .call(
            "DELETE",
            &format!("/api/auth/sessions/{desktop_id}"),
            Some(&laptop),
            None,
        )
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    let own_id = current[0]["id"].as_str().unwrap();
    let own = client
        .call(
            "DELETE",
            &format!("/api/auth/sessions/{own_id}"),
            Some(&laptop),
            None,
        )
        .await;
    assert_eq!(own.status, StatusCode::BAD_REQUEST, "sign out instead");

    let others = client
        .call("DELETE", "/api/auth/sessions", Some(&laptop), None)
        .await;
    assert_eq!(others.body["sessions"].as_array().unwrap().len(), 1);
    assert!(!signed_in(&client, &setup).await);
    assert!(signed_in(&client, &laptop).await);
}

#[tokio::test]
async fn administrators_sign_accounts_out_everywhere() {
    let client = Client::new().await;
    let root = client.setup().await;
    client.create(&root, "ada", "admin").await;
    client.create(&root, "bob", "user").await;
    let ada = client.login("ada", "a good password").await.cookie.unwrap();
    let bob1 = client.login("bob", "a good password").await.cookie.unwrap();
    let bob2 = client.login("bob", "a good password").await.cookie.unwrap();
    let (root_id, bob_id) = (
        client.id_of(&root, "root").await,
        client.id_of(&root, "bob").await,
    );

    let path = format!("/api/users/{bob_id}/sessions");
    assert_eq!(sessions(&client, &ada, &path).await.len(), 2);
    assert_eq!(
        client
            .call(
                "GET",
                &format!("/api/users/{root_id}/sessions"),
                Some(&ada),
                None
            )
            .await
            .status,
        StatusCode::FORBIDDEN,
        "Admins don't look at Super Admins"
    );
    assert_eq!(
        client.call("GET", &path, Some(&bob1), None).await.status,
        StatusCode::FORBIDDEN,
        "users don't list accounts' sessions"
    );

    let out = client.call("DELETE", &path, Some(&ada), None).await;
    assert_eq!(out.status, StatusCode::OK);
    assert_eq!(out.body["sessions"], json!([]));
    assert!(!signed_in(&client, &bob1).await && !signed_in(&client, &bob2).await);
    assert_eq!(
        client
            .call(
                "DELETE",
                &format!("/api/users/{root_id}/sessions"),
                Some(&root),
                None
            )
            .await
            .status,
        StatusCode::FORBIDDEN,
        "your own sessions end under Profile"
    );
}

#[tokio::test]
async fn people_name_themselves_and_lists_show_groups() {
    let client = Client::new().await;
    let root = client.setup().await;
    client.create(&root, "bob", "user").await;
    let bob = client.login("bob", "a good password").await.cookie.unwrap();

    let named = client
        .call(
            "PATCH",
            "/api/auth/profile",
            Some(&bob),
            Some(json!({"displayName": "  Bob Builder "})),
        )
        .await;
    assert_eq!(named.status, StatusCode::OK);
    assert_eq!(named.body["user"]["displayName"], "Bob Builder");
    let bad = client
        .call(
            "PATCH",
            "/api/auth/profile",
            Some(&bob),
            Some(json!({"displayName": "two\nlines"})),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    let bob_id = client.id_of(&root, "bob").await;
    let group = client
        .call(
            "POST",
            "/api/groups",
            Some(&root),
            Some(json!({"name": "Staff"})),
        )
        .await;
    let group_id = group.body["group"]["id"].as_i64().unwrap();
    client
        .call(
            "PATCH",
            &format!("/api/groups/{group_id}"),
            Some(&root),
            Some(json!({"memberIds": [bob_id]})),
        )
        .await;
    let list = client.call("GET", "/api/users", Some(&root), None).await;
    let bob_row = list.body["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "bob")
        .unwrap()
        .clone();
    assert_eq!(bob_row["displayName"], "Bob Builder");
    assert_eq!(bob_row["groups"], json!(["Staff"]));
    assert_eq!(bob_row["lockedUntil"], Value::Null);
    assert_eq!(bob_row["failedAttempts"], 0);
}
