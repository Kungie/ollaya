//! Pull against a real HTTP server: a static registry plus a range-capable blob host.

use axum::response::IntoResponse;
use std::sync::{Arc, Mutex};

use ollaya_registry::manifest::{Descriptor, MANIFEST_V2, Manifest, media};
use ollaya_registry::store::sha256_hex;
use ollaya_registry::{Error, ModelName, Progress, Puller, Store};

/// A static registry served from a temp dir, the way the website serves `/v2/...`.
async fn serve(dir: &std::path::Path) -> String {
    let app = axum::Router::new().fallback_service(tower_http::services::ServeDir::new(dir));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn descriptor(media_type: &str, bytes: &[u8], url: String) -> Descriptor {
    Descriptor {
        media_type: media_type.into(),
        digest: format!("sha256:{}", sha256_hex(bytes)),
        size: bytes.len() as u64,
        urls: vec![url],
        annotations: Default::default(),
    }
}

/// Publish `weights` + a config under `library/test:<tag>`; weights are hosted "elsewhere" (a
/// different path, like a Hugging Face repo), referenced through `urls`.
fn publish(
    root: &std::path::Path,
    base: &str,
    tag: &str,
    weights: &[u8],
    advertised: Option<&[u8]>,
) {
    std::fs::create_dir_all(root.join("hf")).unwrap();
    std::fs::write(root.join("hf/model.safetensors"), weights).unwrap();
    let config = br#"{"model_format":"onnx","family":"test"}"#;
    std::fs::write(root.join("hf/config.json"), config).unwrap();
    let manifest = Manifest {
        schema_version: 2,
        media_type: MANIFEST_V2.into(),
        config: descriptor(media::CONFIG, config, format!("{base}/hf/config.json")),
        layers: vec![descriptor(
            media::WEIGHTS,
            advertised.unwrap_or(weights),
            format!("{base}/hf/model.safetensors"),
        )],
    };
    let dir = root.join("v2/library/test/manifests");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(tag), serde_json::to_vec(&manifest).unwrap()).unwrap();
}

fn pseudo_random(n: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e3779b97f4a7c15;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[tokio::test]
async fn falls_back_to_a_single_stream_when_the_server_ignores_range() {
    // A mirror (like Artifactory's `/resolve/`) answers `Range:` with the whole body as `200`
    // instead of `206`. The puller must stream the blob once, not fail.
    let weights = Arc::new(pseudo_random(40 << 20)); // above the single-stream limit
    let remote = tempfile::tempdir().unwrap();
    let app = {
        let weights = weights.clone();
        axum::Router::new()
            .route(
                "/hf/model.safetensors",
                axum::routing::get(move || {
                    let weights = weights.clone();
                    async move { weights.as_ref().clone() }
                }),
            )
            .fallback_service(tower_http::services::ServeDir::new(remote.path()))
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // Republish with the real base so the manifest references the live server.
    publish(remote.path(), &base, "latest", &weights, None);
    let local = tempfile::tempdir().unwrap();
    let store = Store::open(local.path()).unwrap();
    let puller = Puller::new(store.clone()).unwrap();
    let name = ModelName::parse(&format!("{base}/library/test")).unwrap();
    let digest = format!("sha256:{}", sha256_hex(&weights));

    puller.pull(&name, &|_| {}).await.unwrap();
    let blob = store.blob_path(&digest).unwrap();
    assert_eq!(std::fs::read(&blob).unwrap(), &weights[..]);
}

#[tokio::test]
async fn retries_a_408_timeout_and_completes() {
    // A mirror (or proxy) that answers one 408 Request Timeout before serving the blob must not
    // fail the pull: 408 is transient, so the request is retried with backoff. Here that is the
    // range request, whose 200 then falls back to a single stream.
    let weights = Arc::new(pseudo_random(40 << 20)); // above the single-stream limit
    let remote = tempfile::tempdir().unwrap();
    let app = {
        let weights = weights.clone();
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        axum::Router::new()
            .route(
                "/hf/model.safetensors",
                axum::routing::get(move || {
                    let weights = weights.clone();
                    let first = first.clone();
                    async move {
                        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                            (axum::http::StatusCode::REQUEST_TIMEOUT, "").into_response()
                        } else {
                            weights.as_ref().clone().into_response()
                        }
                    }
                }),
            )
            .fallback_service(tower_http::services::ServeDir::new(remote.path()))
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    publish(remote.path(), &base, "latest", &weights, None);
    let local = tempfile::tempdir().unwrap();
    let store = Store::open(local.path()).unwrap();
    let puller = Puller::new(store.clone()).unwrap();
    let name = ModelName::parse(&format!("{base}/library/test")).unwrap();
    let digest = format!("sha256:{}", sha256_hex(&weights));

    puller.pull(&name, &|_| {}).await.unwrap();
    let blob = store.blob_path(&digest).unwrap();
    assert_eq!(std::fs::read(&blob).unwrap(), &weights[..]);
}

#[tokio::test]
async fn pulls_ranged_blob_and_resumes() {
    let remote = tempfile::tempdir().unwrap();
    let base = serve(remote.path()).await;
    let weights = pseudo_random(40 << 20); // above the single-stream limit: parallel ranges
    publish(remote.path(), &base, "latest", &weights, None);

    let local = tempfile::tempdir().unwrap();
    let store = Store::open(local.path()).unwrap();
    let puller = Puller::new(store.clone()).unwrap();
    let name = ModelName::parse(&format!("{base}/library/test")).unwrap();

    // Simulate an interrupted earlier pull: half of the first part already on disk.
    let digest = format!("sha256:{}", sha256_hex(&weights));
    let blob = store.blob_path(&digest).unwrap();
    let partial = format!("{}-partial", blob.display());
    let mut pre = vec![0u8; weights.len()];
    pre[..1 << 20].copy_from_slice(&weights[..1 << 20]);
    std::fs::write(&partial, &pre).unwrap();
    let part = (weights.len() as u64).div_ceil(1);
    std::fs::write(
        format!("{partial}.json"),
        format!(r#"[{{"offset":0,"size":{part},"completed":{}}}]"#, 1 << 20),
    )
    .unwrap();

    let seen = Mutex::new(Vec::<Progress>::new());
    let manifest = puller
        .pull(&name, &|p| seen.lock().unwrap().push(p))
        .await
        .unwrap();

    assert_eq!(std::fs::read(&blob).unwrap(), weights);
    assert!(!std::path::Path::new(&partial).exists());
    let entry = store.read_manifest(&name).unwrap().unwrap();
    assert_eq!(entry.manifest, manifest);
    let statuses: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|p| p.status.clone())
        .collect();
    assert_eq!(
        statuses.first().map(String::as_str),
        Some("pulling manifest")
    );
    assert_eq!(statuses.last().map(String::as_str), Some("success"));
    let last_blob = seen
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|p| p.digest.as_deref() == Some(&digest))
        .cloned()
        .unwrap();
    assert_eq!(last_blob.completed, Some(weights.len() as u64));

    // Pulling again downloads nothing.
    puller.pull(&name, &|_| {}).await.unwrap();
}

#[tokio::test]
async fn rejects_digest_mismatch_and_writes_no_manifest() {
    let remote = tempfile::tempdir().unwrap();
    let base = serve(remote.path()).await;
    let weights = pseudo_random(1 << 20);
    let mut tampered = weights.clone();
    tampered[0] ^= 1;
    publish(remote.path(), &base, "bad", &tampered, Some(&weights));

    let local = tempfile::tempdir().unwrap();
    let store = Store::open(local.path()).unwrap();
    let puller = Puller::new(store.clone()).unwrap();
    let name = ModelName::parse(&format!("{base}/library/test:bad")).unwrap();
    let err = puller.pull(&name, &|_| {}).await.unwrap_err();
    assert!(matches!(err, Error::DigestMismatch { .. }), "{err}");
    assert!(store.read_manifest(&name).unwrap().is_none());
    assert!(store.list().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_model_is_not_found() {
    let remote = tempfile::tempdir().unwrap();
    let base = serve(remote.path()).await;
    let local = tempfile::tempdir().unwrap();
    let puller = Puller::new(Store::open(local.path()).unwrap()).unwrap();
    let err = puller
        .pull(
            &ModelName::parse(&format!("{base}/library/nope")).unwrap(),
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err}");
}

#[tokio::test]
async fn a_model_this_client_cannot_run_downloads_nothing_but_its_config() {
    let remote = tempfile::tempdir().unwrap();
    let base = serve(remote.path()).await;
    let weights = pseudo_random(1 << 20);
    publish(remote.path(), &base, "latest", &weights, None);

    let local = tempfile::tempdir().unwrap();
    let store = Store::open(local.path()).unwrap();
    let check: ollaya_registry::pull::RunCheck = std::sync::Arc::new(|_, c| {
        if c.model_format == "onnx" {
            Err(format!("no {} engine here", c.model_format))
        } else {
            Ok(())
        }
    });
    let puller = Puller::new(store.clone()).unwrap().with_check(check);
    let name = ModelName::parse(&format!("{base}/library/test")).unwrap();
    let err = puller.pull(&name, &|_| {}).await.unwrap_err();
    assert!(
        matches!(&err, Error::Unsupported(m) if m == "no onnx engine here"),
        "{err}"
    );
    let weights_blob = store
        .blob_path(&format!("sha256:{}", sha256_hex(&weights)))
        .unwrap();
    assert!(!weights_blob.exists());
    assert!(store.read_manifest(&name).unwrap().is_none());

    // A check that accepts the model pulls it as before.
    let accept: ollaya_registry::pull::RunCheck = std::sync::Arc::new(|_, _| Ok(()));
    let puller = Puller::new(store.clone()).unwrap().with_check(accept);
    puller.pull(&name, &|_| {}).await.unwrap();
    assert!(weights_blob.exists());
}

// Pulls by digest (`name@sha256:<hex>`), against a registry laid out like the website's.

/// The website's registry: static files, with its rewrite for manifests by digest
/// (`site/public/_redirects`): `/v2/<ns>/<model>/manifests/sha256:<hex>` is served from the file
/// `sha256-<hex>`, since a colon can't be in a file name on Windows. A missing manifest is the
/// site's HTML 404 page, or, with `html_200`, an HTML page with status 200, as some static hosts
/// answer any path.
async fn serve_site(dir: &std::path::Path, html_200: bool) -> String {
    use axum::http::{StatusCode, header};
    let root = dir.to_owned();
    let manifests = move |axum::extract::Path(path): axum::extract::Path<String>| {
        let root = root.clone();
        async move {
            let path = path.replacen("/manifests/sha256:", "/manifests/sha256-", 1);
            match std::fs::read(root.join("v2").join(path)) {
                Ok(bytes) => bytes.into_response(),
                Err(_) => (
                    if html_200 {
                        StatusCode::OK
                    } else {
                        StatusCode::NOT_FOUND
                    },
                    [(header::CONTENT_TYPE, "text/html")],
                    "<!DOCTYPE html><html><title>Page not found</title></html>",
                )
                    .into_response(),
            }
        }
    };
    let app = axum::Router::new()
        .route("/v2/{*path}", axum::routing::get(manifests))
        .fallback_service(tower_http::services::ServeDir::new(dir));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Keep the manifest `tag` points to now as `sha256-<hex>`, as `package.py` does when it writes
/// a tag. Returns its digest.
fn retain(root: &std::path::Path, tag: &str) -> String {
    let dir = root.join("v2/library/test/manifests");
    let bytes = std::fs::read(dir.join(tag)).unwrap();
    let hex = sha256_hex(&bytes);
    std::fs::write(dir.join(format!("sha256-{hex}")), &bytes).unwrap();
    format!("sha256:{hex}")
}

/// Publish `weights` under `test:<tag>` at its own path, so that versions don't overwrite each
/// other's weights (as two Hugging Face commits don't), and keep the manifest by digest.
fn publish_version(root: &std::path::Path, base: &str, tag: &str, weights: &[u8]) -> String {
    let hex = sha256_hex(weights);
    std::fs::create_dir_all(root.join("hf")).unwrap();
    std::fs::write(root.join(format!("hf/{hex}.safetensors")), weights).unwrap();
    let config = br#"{"model_format":"onnx","family":"test"}"#;
    std::fs::write(root.join("hf/config.json"), config).unwrap();
    let manifest = Manifest {
        schema_version: 2,
        media_type: MANIFEST_V2.into(),
        config: descriptor(media::CONFIG, config, format!("{base}/hf/config.json")),
        layers: vec![descriptor(
            media::WEIGHTS,
            weights,
            format!("{base}/hf/{hex}.safetensors"),
        )],
    };
    let dir = root.join("v2/library/test/manifests");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(tag), serde_json::to_vec(&manifest).unwrap()).unwrap();
    retain(root, tag)
}

fn local_digest(store: &Store, name: &str) -> Option<String> {
    store
        .read_manifest(&ModelName::parse(name).unwrap())
        .unwrap()
        .map(|e| e.digest)
}

#[tokio::test]
async fn pulls_by_digest_alone_and_stores_under_the_digest_tag() {
    let remote = tempfile::tempdir().unwrap();
    let base = serve_site(remote.path(), false).await;
    let weights = pseudo_random(1 << 20);
    let digest = publish_version(remote.path(), &base, "en", &weights);

    let local = tempfile::tempdir().unwrap();
    let store = Store::open(local.path()).unwrap();
    let puller = Puller::new(store.clone()).unwrap();
    let name = ModelName::parse(&format!("{base}/library/test@{digest}")).unwrap();
    puller.pull(&name, &|_| {}).await.unwrap();

    let hex = &digest["sha256:".len()..];
    let path = store.manifest_path(&name);
    assert!(path.ends_with(format!("test/sha256-{hex}")), "{path:?}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        std::fs::read(remote.path().join("v2/library/test/manifests/en")).unwrap()
    );
    let entries = store.list().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].digest, digest);
    assert_eq!(entries[0].name.digest.as_ref(), Some(&digest));
    assert!(
        entries[0]
            .name
            .to_string()
            .ends_with(&format!("/library/test@{digest}"))
    );
    let weights_blob = store
        .blob_path(&format!("sha256:{}", sha256_hex(&weights)))
        .unwrap();
    assert_eq!(std::fs::read(weights_blob).unwrap(), weights);
    // The tag it was published under was not touched.
    assert_eq!(
        local_digest(&store, &format!("{base}/library/test:en")),
        None
    );
}

#[tokio::test]
async fn a_moved_tag_still_pulls_the_pinned_manifest_under_the_tag() {
    let remote = tempfile::tempdir().unwrap();
    let base = serve_site(remote.path(), false).await;
    let (w1, w2) = (pseudo_random(1 << 20), pseudo_random(3 << 19));
    let d1 = publish_version(remote.path(), &base, "en", &w1);
    let d2 = publish_version(remote.path(), &base, "en", &w2); // the tag moves
    assert_ne!(d1, d2);

    let local = tempfile::tempdir().unwrap();
    let store = Store::open(local.path()).unwrap();
    let puller = Puller::new(store.clone()).unwrap();
    let en = format!("{base}/library/test:en");

    // By tag: the tag's manifest now.
    puller
        .pull(&ModelName::parse(&en).unwrap(), &|_| {})
        .await
        .unwrap();
    assert_eq!(local_digest(&store, &en), Some(d2.clone()));

    // By tag and digest: the old manifest, under the tag, with its own blobs.
    let pinned = ModelName::parse(&format!("{en}@{d1}")).unwrap();
    let manifest = puller.pull(&pinned, &|_| {}).await.unwrap();
    assert_eq!(local_digest(&store, &en), Some(d1.clone()));
    assert!(store.read_manifest(&pinned).unwrap().is_some());
    assert!(manifest.blobs().all(|d| store.has_blob(d)));
    let w1_blob = store
        .blob_path(&format!("sha256:{}", sha256_hex(&w1)))
        .unwrap();
    assert_eq!(std::fs::read(w1_blob).unwrap(), w1);

    // Pulling it again downloads nothing and changes nothing; a pull by tag moves it on.
    let seen = Mutex::new(Vec::<Progress>::new());
    puller
        .pull(&pinned, &|p| seen.lock().unwrap().push(p))
        .await
        .unwrap();
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|p| p.digest.is_some())
            .all(|p| p.completed == p.total)
    );
    assert_eq!(local_digest(&store, &en), Some(d1));
    puller
        .pull(&ModelName::parse(&en).unwrap(), &|_| {})
        .await
        .unwrap();
    assert_eq!(local_digest(&store, &en), Some(d2));
}

#[tokio::test]
async fn the_tag_of_a_pinned_pull_is_where_it_is_stored() {
    // `name:tag@digest` is not checked against the registry's tags: the digest says which
    // manifest, the tag where it goes. A registry serves a model's manifests by digest whatever
    // tag published them.
    let remote = tempfile::tempdir().unwrap();
    let base = serve_site(remote.path(), false).await;
    let d1 = publish_version(remote.path(), &base, "en", &pseudo_random(1 << 20));

    let local = tempfile::tempdir().unwrap();
    let store = Store::open(local.path()).unwrap();
    let puller = Puller::new(store.clone()).unwrap();
    let pinned = format!("{base}/library/test:pinned@{d1}");
    puller
        .pull(&ModelName::parse(&pinned).unwrap(), &|_| {})
        .await
        .unwrap();
    assert_eq!(
        local_digest(&store, &format!("{base}/library/test:pinned")),
        Some(d1)
    );
    assert!(
        !remote
            .path()
            .join("v2/library/test/manifests/pinned")
            .exists()
    );
}

#[tokio::test]
async fn a_manifest_that_does_not_match_its_digest_is_refused_before_any_download() {
    let remote = tempfile::tempdir().unwrap();
    let base = serve_site(remote.path(), false).await;
    let (w1, w2) = (pseudo_random(1 << 20), pseudo_random(3 << 19));
    let d1 = publish_version(remote.path(), &base, "en", &w1);
    let d2 = publish_version(remote.path(), &base, "en", &w2);

    let local = tempfile::tempdir().unwrap();
    let store = Store::open(local.path()).unwrap();
    let puller = Puller::new(store.clone()).unwrap();
    let en = format!("{base}/library/test:en");
    puller
        .pull(&ModelName::parse(&en).unwrap(), &|_| {})
        .await
        .unwrap();
    assert_eq!(local_digest(&store, &en), Some(d2.clone()));

    // The registry serves another manifest (here the current one) under d1's name.
    let dir = remote.path().join("v2/library/test/manifests");
    std::fs::copy(
        dir.join(format!("sha256-{}", &d2[7..])),
        dir.join(format!("sha256-{}", &d1[7..])),
    )
    .unwrap();
    let seen = Mutex::new(Vec::<Progress>::new());
    let err = puller
        .pull(&ModelName::parse(&format!("{en}@{d1}")).unwrap(), &|p| {
            seen.lock().unwrap().push(p)
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::ManifestMismatch { got, .. } if *got == d2),
        "{err}"
    );
    assert!(err.to_string().contains("nothing was stored"), "{err}");
    // Nothing downloaded, and the tag still holds what it held.
    assert!(seen.lock().unwrap().iter().all(|p| p.digest.is_none()));
    let w1_blob = store
        .blob_path(&format!("sha256:{}", sha256_hex(&w1)))
        .unwrap();
    assert!(!w1_blob.exists());
    assert_eq!(local_digest(&store, &en), Some(d2));
    assert_eq!(store.list().unwrap().len(), 1);
}

#[tokio::test]
async fn an_unknown_digest_is_its_own_error_not_an_html_page() {
    let unknown = format!("sha256:{}", "0".repeat(64));
    for html_200 in [false, true] {
        let remote = tempfile::tempdir().unwrap();
        let base = serve_site(remote.path(), html_200).await;
        publish_version(remote.path(), &base, "en", &pseudo_random(1 << 20));
        let local = tempfile::tempdir().unwrap();
        let store = Store::open(local.path()).unwrap();
        let puller = Puller::new(store.clone()).unwrap();
        let name = ModelName::parse(&format!("{base}/library/test:en@{unknown}")).unwrap();
        let err = puller.pull(&name, &|_| {}).await.unwrap_err();
        if html_200 {
            // A host that answers every path: the page is not the manifest the digest names.
            assert!(matches!(err, Error::ManifestMismatch { .. }), "{err}");
        } else {
            assert!(
                matches!(&err, Error::DigestNotFound(n) if *n == name.to_string()),
                "{err}"
            );
            assert!(
                err.to_string()
                    .ends_with("not found: the registry has no manifest with that digest"),
                "{err}"
            );
        }
        assert!(store.list().unwrap().is_empty());
    }
}
