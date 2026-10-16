use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
    Json, Router,
};
use fpv_catalog_bot::immich::ImmichClient;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
#[derive(Default)]
struct Mock {
    search: Value,
    requests: Vec<Value>,
    deleted: Vec<String>,
    response_overrides: Value,
}
async fn search(
    State(s): State<Arc<Mutex<Mock>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    assert_eq!(headers["x-api-key"], "test-only");
    s.lock().unwrap().requests.push(body);
    Json(s.lock().unwrap().search.clone())
}
async fn share(State(s): State<Arc<Mutex<Mock>>>, Json(body): Json<Value>) -> Json<Value> {
    let mut s = s.lock().unwrap();
    s.requests.push(body.clone());
    let mut response = json!({"id":"77777777-7777-4777-8777-777777777777","key":"random_secret","expiresAt":body["expiresAt"],
        "allowDownload":body["allowDownload"],"allowUpload":body["allowUpload"],"showMetadata":body["showMetadata"],
        "assets":body["assetIds"].as_array().unwrap().iter().map(|id|json!({"id":id})).collect::<Vec<_>>()});
    if let Some(overrides) = s.response_overrides.as_object() {
        response.as_object_mut().unwrap().extend(overrides.clone());
    }
    Json(response)
}
async fn remove(State(s): State<Arc<Mutex<Mock>>>, Path(id): Path<String>) -> StatusCode {
    s.lock().unwrap().deleted.push(id);
    StatusCode::NO_CONTENT
}
async fn mock() -> (ImmichClient, Arc<Mutex<Mock>>, tokio::task::JoinHandle<()>) {
    let s = Arc::new(Mutex::new(Mock::default()));
    let app = Router::new()
        .route("/api/search/metadata", post(search))
        .route(
            "/api/shared-links",
            post(share).get(|| async { Json(json!([])) }),
        )
        .route("/api/shared-links/:id", delete(remove))
        .route("/api/server/ping", get(|| async { StatusCode::OK }))
        .with_state(s.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = ImmichClient::new(
        format!("http://{addr}/api"),
        "https://photos.example".into(),
        "test-only".into(),
        "/mnt/fpv".into(),
        "11111111-1111-4111-8111-111111111111".into(),
    );
    (client, s, task)
}
fn asset(path: &str, library: &str) -> Value {
    json!({"id":"22222222-2222-4222-8222-222222222222","originalPath":path,"libraryId":library,"isOffline":false,"isTrashed":false,"type":"VIDEO"})
}
#[tokio::test]
async fn resolves_exact_path_and_checks_library() {
    let (c, s, t) = mock().await;
    s.lock().unwrap().search = json!({"assets":{"items":[asset("/mnt/fpv/событие/v.mp4","11111111-1111-4111-8111-111111111111")],"nextCursor":null}});
    assert!(c.resolve_asset("событие/v.mp4").await.unwrap().is_some());
    let body = &s.lock().unwrap().requests[0];
    assert_eq!(
        body["filter"]["originalPath"]["eq"],
        "/mnt/fpv/событие/v.mp4"
    );
    t.abort();
}
#[tokio::test]
async fn rejects_wrong_path_library_offline_and_duplicates() {
    let (c, s, t) = mock().await;
    let path = "/mnt/fpv/e/v.mp4";
    for invalid in [
        asset(path, "33333333-3333-4333-8333-333333333333"),
        asset("/other/v.mp4", "11111111-1111-4111-8111-111111111111"),
        {
            let mut a = asset(path, "11111111-1111-4111-8111-111111111111");
            a["isOffline"] = json!(true);
            a
        },
    ] {
        s.lock().unwrap().search = json!({"assets":{"items":[invalid],"nextCursor":null}});
        assert!(c.resolve_asset("e/v.mp4").await.unwrap().is_none());
    }
    let a = asset(path, "11111111-1111-4111-8111-111111111111");
    s.lock().unwrap().search = json!({"assets":{"items":[a.clone(),a],"nextCursor":null}});
    assert!(c.resolve_asset("e/v.mp4").await.is_err());
    assert!(c.resolve_asset("../outside").await.is_err());
    t.abort();
}
#[tokio::test]
async fn share_flags_expiry_and_revoke() {
    let (c, s, t) = mock().await;
    let ids = [Uuid::new_v4(), Uuid::new_v4()];
    let expiry = chrono::Utc::now() + chrono::Duration::days(7);
    let (share, url) = c
        .create_share(&ids, expiry, "test request".into())
        .await
        .unwrap();
    assert_eq!(url, "https://photos.example/share/random_secret");
    let body = s.lock().unwrap().requests[0].clone();
    assert_eq!(body["allowDownload"], true);
    assert_eq!(body["allowUpload"], false);
    assert_eq!(body["showMetadata"], false);
    assert_eq!(body["type"], "INDIVIDUAL");
    assert_eq!(body["assetIds"].as_array().unwrap().len(), 2);
    assert_eq!(share.expires_at.unwrap(), expiry);
    c.revoke_share(share.id).await.unwrap();
    assert_eq!(s.lock().unwrap().deleted.len(), 1);
    t.abort();
}

#[tokio::test]
async fn share_metadata_can_be_enabled_for_immich_downloads() {
    let (c, s, t) = mock().await;
    let c = c.with_share_metadata(true);
    c.create_share(
        &[Uuid::new_v4()],
        chrono::Utc::now() + chrono::Duration::days(7),
        "metadata allowed".into(),
    )
    .await
    .unwrap();
    let request = s.lock().unwrap().requests[0].clone();
    assert_eq!(request["showMetadata"], true);
    assert_eq!(request["allowDownload"], true);
    assert_eq!(request["allowUpload"], false);
    t.abort();
}

#[tokio::test]
async fn incompatible_share_is_revoked_instead_of_published() {
    let (c, s, t) = mock().await;
    let ids = [Uuid::new_v4()];
    let expiry = chrono::Utc::now() + chrono::Duration::days(7);
    let overrides = [
        json!({"allowDownload":false}), // Actual Immich behavior with hidden metadata.
        json!({"allowUpload":true}),
        json!({"showMetadata":true}),
        json!({"assets":[]}),
        json!({"assets":[{"id":ids[0]},{"id":Uuid::new_v4()}]}),
        json!({"assets":[{"id":ids[0]},{"id":"invalid"}]}),
        json!({"expiresAt":null}),
        json!({"expiresAt":expiry + chrono::Duration::days(1)}),
    ];
    for value in &overrides {
        s.lock().unwrap().response_overrides = value.clone();
        let error = c
            .create_share(&ids, expiry, "invalid response".into())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("link revoked"));
    }
    assert_eq!(s.lock().unwrap().deleted.len(), overrides.len());
    t.abort();
}
