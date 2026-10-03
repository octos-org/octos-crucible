//! GitHub Release store against a mock API: create-on-demand, upload,
//! idempotent put, verified download. No real repository is touched.

use crucible_store::{BlobStore, GithubReleaseStore, StoreError, release_tag, sha256_hex};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const REPO: &str = "octos-org/test-blobs";
const DATA: &[u8] = b"sealed bytes";

fn store(server: &MockServer) -> GithubReleaseStore {
    GithubReleaseStore::with_api_base(REPO, "test-token".into(), &server.uri()).unwrap()
}

fn release_json(server: &MockServer, id: u64) -> serde_json::Value {
    json!({"id": id, "upload_url": format!("{}/upload/{id}/assets{{?name,label}}", server.uri())})
}

#[tokio::test]
async fn put_creates_missing_release_and_uploads() {
    let server = MockServer::start().await;
    let hash = sha256_hex(DATA);
    let tag = release_tag(&hash).unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/releases/tags/{tag}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/{REPO}/releases")))
        .and(header("authorization", "Bearer test-token"))
        .and(body_partial_json(
            json!({"tag_name": tag, "prerelease": true}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(release_json(&server, 7)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/releases/7/assets")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/upload/7/assets"))
        .and(query_param("name", hash.as_str()))
        .and(header("content-type", "application/octet-stream"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 99, "name": hash})))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(store(&server).put(DATA).await.unwrap(), hash);
    let uploads: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/upload/7/assets")
        .collect();
    assert_eq!(uploads[0].body, DATA);
}

#[tokio::test]
async fn put_is_idempotent() {
    let server = MockServer::start().await;
    let hash = sha256_hex(DATA);
    let tag = release_tag(&hash).unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/releases/tags/{tag}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(release_json(&server, 7)))
        .expect(1) // cached after the first lookup
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/releases/7/assets")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id": 1, "name": "other"}, {"id": 99, "name": hash}])),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let s = store(&server);
    assert_eq!(s.put(DATA).await.unwrap(), hash);
    assert_eq!(s.put(DATA).await.unwrap(), hash);
    assert!(s.exists(&hash).await.unwrap());
}

#[tokio::test]
async fn get_downloads_and_verifies() {
    let server = MockServer::start().await;
    let hash = sha256_hex(DATA);
    let tag = release_tag(&hash).unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/releases/tags/{tag}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(release_json(&server, 7)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/releases/7/assets")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 99, "name": hash}])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/releases/assets/99")))
        .and(header("accept", "application/octet-stream"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(DATA))
        .mount(&server)
        .await;
    let s = store(&server);
    assert_eq!(s.get(&hash).await.unwrap(), DATA);

    // Same shard, listed under its name, but the bytes do not match.
    let server2 = MockServer::start().await;
    for m in [
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/releases/tags/{tag}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_json(&server2, 7))),
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/releases/7/assets")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([{"id": 99, "name": hash}])),
            ),
        Mock::given(method("GET"))
            .and(path(format!("/repos/{REPO}/releases/assets/99")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(&b"tampered"[..])),
    ] {
        m.mount(&server2).await;
    }
    assert!(matches!(
        store(&server2).get(&hash).await,
        Err(StoreError::Corrupt(_))
    ));
}

#[tokio::test]
async fn missing_release_means_missing_blob() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let s = store(&server);
    let hash = sha256_hex(DATA);
    assert!(!s.exists(&hash).await.unwrap());
    assert!(matches!(s.get(&hash).await, Err(StoreError::NotFound(_))));
    assert!(matches!(
        s.get("not-a-hash").await,
        Err(StoreError::BadHash(_))
    ));
}

#[tokio::test]
async fn paginates_assets() {
    let server = MockServer::start().await;
    let hash = sha256_hex(DATA);
    let tag = release_tag(&hash).unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/releases/tags/{tag}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(release_json(&server, 7)))
        .mount(&server)
        .await;
    let full: Vec<_> = (0..100)
        .map(|i| json!({"id": i, "name": format!("x{i}")}))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/releases/7/assets")))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(full))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{REPO}/releases/7/assets")))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id": 500, "name": hash}])))
        .mount(&server)
        .await;
    assert!(store(&server).exists(&hash).await.unwrap());
}
