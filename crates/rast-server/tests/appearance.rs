//! Appearance end to end: each account keeps its own colour mode, accent and
//! wallpaper, a change touches only the fields it names, and bad values are refused.

mod common;

use axum::http::StatusCode;
use common::Client;
use serde_json::json;

#[tokio::test]
async fn each_account_keeps_its_own_appearance() {
    let client = Client::new().await;
    let root = client.setup().await;
    assert_eq!(
        client.create(&root, "bob", "user").await.status,
        StatusCode::CREATED
    );
    let bob = client.login("bob", "a good password").await.cookie.unwrap();

    let first = client
        .call("GET", "/api/appearance", Some(&root), None)
        .await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(
        first.body["appearance"],
        json!({"theme": "light", "accent": "ocean", "wallpaper": {
            "kind": "builtin", "builtinId": "alpine-dawn", "color": "#1B2430",
            "fit": "cover", "position": "center", "overlay": 20}}),
        "classic RAST's defaults until the account saves its own"
    );

    let saved = client
        .call(
            "PUT",
            "/api/appearance",
            Some(&root),
            Some(
                json!({"theme": "dark", "wallpaper": {"builtinId": "forest-mist", "overlay": 35}}),
            ),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let again = client
        .call("GET", "/api/appearance", Some(&root), None)
        .await;
    assert_eq!(again.body, saved.body);
    assert_eq!(again.body["appearance"]["theme"], "dark");
    assert_eq!(again.body["appearance"]["accent"], "ocean", "untouched");
    assert_eq!(
        again.body["appearance"]["wallpaper"]["builtinId"],
        "forest-mist"
    );
    assert_eq!(again.body["appearance"]["wallpaper"]["overlay"], 35);
    assert_eq!(
        again.body["appearance"]["wallpaper"]["fit"], "cover",
        "untouched"
    );

    let theirs = client
        .call("GET", "/api/appearance", Some(&bob), None)
        .await;
    assert_eq!(
        theirs.body["appearance"]["theme"], "light",
        "another account keeps its own"
    );
}

#[tokio::test]
async fn bad_or_unsigned_changes_are_refused() {
    let client = Client::new().await;
    let root = client.setup().await;
    let unsigned = client.call("GET", "/api/appearance", None, None).await;
    assert_eq!(unsigned.status, StatusCode::UNAUTHORIZED);

    for bad in [
        json!({"accent": "neon"}),
        json!({"theme": "dark", "wallpaper": {"overlay": 80}}),
        json!({"wallpaper": {"color": "blue"}}),
    ] {
        let reply = client
            .call("PUT", "/api/appearance", Some(&root), Some(bad.clone()))
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{bad}");
        assert!(reply.body["error"].is_string());
    }
    let unchanged = client
        .call("GET", "/api/appearance", Some(&root), None)
        .await;
    assert_eq!(
        unchanged.body["appearance"]["theme"], "light",
        "a refused change saves nothing"
    );

    let unmarked = client
        .call_with(
            "PUT",
            "/api/appearance",
            Some(&root),
            Some(json!({"theme": "dark"})),
            false,
        )
        .await;
    assert_eq!(
        unmarked.status,
        StatusCode::FORBIDDEN,
        "changes need the RAST header"
    );
}
