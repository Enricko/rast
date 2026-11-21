//! Groups and app rules end to end: precedence, who may change them, and the
//! gate in front of app routes.

mod common;

use axum::http::StatusCode;
use common::Client;
use serde_json::{Value, json};

/// A Super Admin `root`, an Admin `ada` and a User `bob`, signed in.
struct World {
    client: Client,
    root: String,
    ada: String,
    bob: String,
    bob_id: i64,
    ada_id: i64,
}

async fn world() -> World {
    let client = Client::new().await;
    let root = client.setup().await;
    let ada_id = client.create(&root, "ada", "admin").await.body["user"]["id"]
        .as_i64()
        .unwrap();
    let bob_id = client.create(&root, "bob", "user").await.body["user"]["id"]
        .as_i64()
        .unwrap();
    let ada = client.login("ada", "a good password").await.cookie.unwrap();
    let bob = client.login("bob", "a good password").await.cookie.unwrap();
    World {
        client,
        root,
        ada,
        bob,
        bob_id,
        ada_id,
    }
}

impl World {
    async fn group(&self, name: &str, members: &[i64], apps: Value) -> i64 {
        let created = self
            .client
            .call(
                "POST",
                "/api/groups",
                Some(&self.root),
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        let id = created.body["group"]["id"].as_i64().unwrap();
        let updated = self
            .client
            .call(
                "PATCH",
                &format!("/api/groups/{id}"),
                Some(&self.root),
                Some(json!({ "memberIds": members, "apps": apps })),
            )
            .await;
        assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
        id
    }

    /// `session`'s own decision for `app`.
    async fn mine(&self, session: &str, app: &str) -> Value {
        let me = self
            .client
            .call("GET", "/api/access/me", Some(session), None)
            .await;
        assert_eq!(me.status, StatusCode::OK, "{}", me.body);
        me.body["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|decision| decision["id"] == app)
            .cloned()
            .unwrap()
    }

    async fn set_access(&self, session: &str, user: i64, body: Value) -> common::Reply {
        self.client
            .call(
                "PATCH",
                &format!("/api/users/{user}/access"),
                Some(session),
                Some(body),
            )
            .await
    }
}

#[tokio::test]
async fn groups_allow_and_deny_apps_and_a_deny_wins() {
    let w = world().await;
    let docker = w.mine(&w.bob, "applications").await;
    assert_eq!(docker["allowed"], false, "Docker is off by default");
    assert_eq!(docker["source"], "default");
    assert_eq!(w.mine(&w.bob, "files").await["allowed"], true);

    let staff = w
        .group("Staff", &[w.bob_id], json!({ "applications": "allow" }))
        .await;
    let docker = w.mine(&w.bob, "applications").await;
    assert_eq!(docker["allowed"], true);
    assert_eq!(docker["source"], "group");
    assert_eq!(docker["groups"], json!(["Staff"]));

    w.group(
        "Interns",
        &[w.bob_id, w.ada_id],
        json!({ "applications": "deny" }),
    )
    .await;
    let docker = w.mine(&w.bob, "applications").await;
    assert_eq!(docker["allowed"], false, "a deny wins over an allow");
    assert_eq!(docker["groups"], json!(["Interns"]));
    assert_eq!(docker["overridden"], true);

    // An allow on the account itself does not beat a group's deny.
    let own = w
        .set_access(
            &w.root,
            w.bob_id,
            json!({ "apps": { "applications": "allow" } }),
        )
        .await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    assert_eq!(own.body["rules"]["applications"], "allow");
    assert_eq!(w.mine(&w.bob, "applications").await["allowed"], false);

    // Leaving Interns lets the allows through again.
    w.set_access(&w.root, w.bob_id, json!({ "groupIds": [staff] }))
        .await;
    let docker = w.mine(&w.bob, "applications").await;
    assert_eq!(docker["allowed"], true);
    assert_eq!(docker["source"], "account");

    // Administrators open every app, whatever their groups say.
    let admin = w.mine(&w.ada, "applications").await;
    assert_eq!(admin["allowed"], true);
    assert_eq!(admin["source"], "administrator");
}

#[tokio::test]
async fn only_super_admins_change_groups_and_access() {
    let w = world().await;
    let id = w.group("Staff", &[w.bob_id], json!({})).await;

    let list = w
        .client
        .call("GET", "/api/groups", Some(&w.ada), None)
        .await;
    assert_eq!(list.status, StatusCode::OK, "admins may read groups");
    assert_eq!(list.body["groups"][0]["memberIds"], json!([w.bob_id]));
    assert_eq!(list.body["apps"].as_array().unwrap().len(), 7);
    assert_eq!(
        w.client
            .call("GET", "/api/groups", Some(&w.bob), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    let by_admin = w
        .client
        .call(
            "POST",
            "/api/groups",
            Some(&w.ada),
            Some(json!({ "name": "Ops" })),
        )
        .await;
    assert_eq!(by_admin.status, StatusCode::FORBIDDEN);
    assert_eq!(
        w.set_access(&w.ada, w.bob_id, json!({ "apps": { "files": "deny" } }))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        w.client
            .call(
                "GET",
                &format!("/api/users/{}/access", w.bob_id),
                Some(&w.ada),
                None
            )
            .await
            .status,
        StatusCode::OK,
        "admins may read an account's access"
    );

    let taken = w
        .client
        .call(
            "POST",
            "/api/groups",
            Some(&w.root),
            Some(json!({ "name": "staff" })),
        )
        .await;
    assert_eq!(taken.status, StatusCode::CONFLICT, "names ignore case");
    let unknown_app = w
        .client
        .call(
            "PATCH",
            &format!("/api/groups/{id}"),
            Some(&w.root),
            Some(json!({ "apps": { "nope": "allow" } })),
        )
        .await;
    assert_eq!(unknown_app.status, StatusCode::BAD_REQUEST);
    let unknown_member = w
        .client
        .call(
            "PATCH",
            &format!("/api/groups/{id}"),
            Some(&w.root),
            Some(json!({ "memberIds": [9999] })),
        )
        .await;
    assert_eq!(unknown_member.status, StatusCode::BAD_REQUEST);

    let deleted = w
        .client
        .call("DELETE", &format!("/api/groups/{id}"), Some(&w.root), None)
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let me = w
        .client
        .call("GET", "/api/access/me", Some(&w.bob), None)
        .await;
    assert_eq!(me.body["groups"], json!([]));
}

#[tokio::test]
async fn deleting_an_account_removes_it_from_its_groups() {
    let w = world().await;
    w.group("Staff", &[w.bob_id, w.ada_id], json!({})).await;
    w.client
        .call(
            "DELETE",
            &format!("/api/users/{}", w.bob_id),
            Some(&w.root),
            None,
        )
        .await;
    let list = w
        .client
        .call("GET", "/api/groups", Some(&w.root), None)
        .await;
    assert_eq!(list.body["groups"][0]["memberIds"], json!([w.ada_id]));
}

#[tokio::test]
async fn the_gate_closes_routes_of_denied_apps() {
    let w = world().await;
    let get = |session: Option<&str>, path: &str| {
        let client = &w.client;
        let session = session.map(str::to_owned);
        let path = path.to_owned();
        async move { client.call("GET", &path, session.as_deref(), None).await }
    };
    // Allowed apps pass the gate (and find no route yet); unknown paths are 404.
    assert_eq!(
        get(Some(&w.bob), "/api/files/list").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(None, "/api/files/list").await.status,
        StatusCode::UNAUTHORIZED
    );
    let docker = get(Some(&w.bob), "/api/docker/containers").await;
    assert_eq!(docker.status, StatusCode::FORBIDDEN, "off by default");
    assert!(docker.body["error"].as_str().unwrap().contains("Docker"));

    w.set_access(&w.root, w.bob_id, json!({ "apps": { "files": "deny" } }))
        .await;
    assert_eq!(
        get(Some(&w.bob), "/api/files/list").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(Some(&w.ada), "/api/docker/containers").await.status,
        StatusCode::NOT_FOUND,
        "administrators pass"
    );
}
