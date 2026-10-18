use super::*;
use axum::{routing::get, Json, Router};

fn tree(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(path)
        .into_iter()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy();
            // SQLite may create an empty WAL and its transient shared-memory index on open.
            // Neither represents retained records. Nonempty WAL data is still compared.
            !name.ends_with(".sqlite3-shm")
                && !(name.ends_with(".sqlite3-wal") && entry.metadata().unwrap().len() == 0)
        })
        .map(|entry| {
            (
                entry.path().strip_prefix(path).unwrap().to_path_buf(),
                sha256(&fs::read(entry.path()).unwrap()),
            )
        })
        .collect()
}

/// Runs an actual old/new production executable against only a synthetic temporary store.
pub(super) async fn verify(root: &Path, binary: &Path, enabled: Option<bool>) {
    assert!(
        binary.is_file(),
        "Production executable required for process gate"
    );
    let before = tree(&root.join("cloud-v1"));
    let owner = Uuid::from_u128(OWNER);
    let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_url = format!("http://{}/joined", provider.local_addr().unwrap());
    let stub = Router::new().route(
        "/joined",
        get(move || async move {
            Json(serde_json::json!({"id": owner.to_string(), "name": "RollbackFixture"}))
        }),
    );
    let provider_task = tokio::spawn(async move {
        axum::serve(provider, stub).await.unwrap();
    });
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reserved.local_addr().unwrap();
    drop(reserved);
    let mut config = include_str!("../../../Config.example.toml")
        .replace("0.0.0.0:6665", &address.to_string())
        .replace(
            "assetsUpdaterEnabled = true",
            "assetsUpdaterEnabled = false",
        );
    if enabled == Some(false) {
        config = config.replace("protobufEnabled = true", "protobufEnabled = false");
    }
    config =
        format!("authProviders = [{{name = \"Fixture\", url = \"{provider_url}\"}}]\n{config}");
    let config_path = root.join("process-config.toml");
    fs::write(&config_path, config).unwrap();
    let mut process = tokio::process::Command::new(binary)
        .current_dir(root)
        .env(crate::CONFIG_ENV, &config_path)
        .env(crate::AVATARS_ENV, root)
        .env(crate::ASSETS_ENV, root.join("assets"))
        .env(crate::LOGS_ENV, root.join("process-logs"))
        .env(
            crate::LOGGER_ENV,
            if enabled.is_some() {
                "sculptor=trace,tower_http=trace"
            } else {
                "info"
            },
        )
        // Startup update checks cannot contact the public network in this test.
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("ALL_PROXY")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .unwrap();
    let base = format!("http://{address}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        assert!(
            process.try_wait().unwrap().is_none(),
            "Production process exited during startup"
        );
        if let Ok(response) = http.get(format!("{base}/health")).send().await {
            if response.status().is_success() {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Production process startup timed out"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let capability = http
        .get(format!("{base}/api/protobuf/v1/capabilities"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        capability.status().as_u16(),
        if enabled == Some(true) { 200 } else { 404 }
    );
    if enabled != Some(true) {
        assert_eq!(
            http.post(format!("{base}/api/protobuf/v1/rpc"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            404
        );
        assert_eq!(
            http.get(format!("{base}/ws/protobuf/v1"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            404
        );
    }
    let challenge = http
        .get(format!("{base}/api//auth/id?username=RollbackFixture"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let token = http
        .get(format!("{base}/api//auth/verify?id={challenge}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    http.get(format!("{base}/api/"))
        .header("token", "REJECTED_PRIVATE_TOKEN_MARKER")
        .send()
        .await
        .unwrap();
    for _ in 0..2 {
        http.get(format!("{base}/api/{owner}"))
            .header("token", &token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    let bytes = http
        .get(format!("{base}/api/{owner}/avatar"))
        .header("token", &token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), figura());
    if enabled.is_none() && std::env::var_os("OMNIMORPH_CLIENT_CLASSPATH_FILE").is_some() {
        crate::api::protobuf::tests::java_client::execute(
            &[
                "top.aihmc.omnimorph.backend2.protocol.CloudAdapterIntegrationTest".into(),
                "--legacy".into(),
                base.clone(),
                token.clone(),
                owner.to_string(),
                root.join(format!("{owner}.moon"))
                    .to_string_lossy()
                    .into_owned(),
            ],
            root,
        )
        .await;
    }
    process.kill().await.unwrap();
    process.wait().await.unwrap();
    provider_task.abort();
    let _ = provider_task.await;
    if enabled.is_some() {
        for entry in fs::read_dir(root.join("process-logs")).unwrap() {
            let entry = entry.unwrap();
            let log = fs::read_to_string(entry.path()).unwrap();
            assert!(
                !log.contains("REJECTED_PRIVATE_TOKEN_MARKER"),
                "Rejected token leaked into backend trace logs"
            );
            // The baseline ran at info; its separate legacy diagnostics never logged this new challenge.
            assert!(
                !log.contains(&token),
                "Authentication response/session token leaked into backend trace logs"
            );
        }
    }
    assert_eq!(
        tree(&root.join("cloud-v1")),
        before,
        "Rollback/toggle must retain all additive records and blobs"
    );
    assert_eq!(
        fs::read(root.join(format!("{owner}.moon"))).unwrap(),
        figura()
    );
    println!("B8 production process gate PASS: {}", match enabled {
        None => "baseline 28fb81e rollback, original Figura auth/profile/raw bytes, untouched cloud-v1",
        Some(false) => "new backend Protobuf disabled, old routes remain, all new data retained",
        Some(true) => "new backend Protobuf enabled, legacy routes and additive data retained",
    });
}
