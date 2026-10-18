use super::*;

#[tokio::test]
async fn production_java_library_client_interoperates_with_the_actual_rust_listener() {
    if std::env::var_os("OMNIMORPH_CLIENT_CLASSPATH_FILE").is_none() {
        println!("SKIPPED cross-language library client: set OMNIMORPH_CLIENT_CLASSPATH_FILE");
        return;
    }
    let server = Server::start().await;
    let (model, bytes) = server.upload_model("PUBLIC").await;
    let response = server.request(Method::PATCH, &format!("/api/library/v1/models/{model}"), 1)
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 2, "requires_authorization": true}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    let fixture = server._directory.path().join("library-package.bin");
    std::fs::write(&fixture, bytes).unwrap();
    crate::api::protobuf::tests::java_client::execute(
        &[
            "top.aihmc.omnimorph.backend2.library.LibraryHttpIntegrationTest".into(),
            server.base.clone(),
            model,
            server.state.storage.instance_id().to_owned(),
            fixture.to_string_lossy().into_owned(),
        ],
        server._directory.path(),
    )
    .await;
}

#[tokio::test]
async fn ten_thousand_models_keep_http_pages_bounded_and_search_includes_tags_and_admin_uploaders()
{
    let server = Server::start().await;
    let owner = Uuid::from_u128(1).to_string();
    let authors = serde_json::to_string(&[
        "长中文作者甲保持完整元数据",
        "长中文作者乙保持完整元数据",
        "长中文作者丙保持完整元数据",
        "长中文作者丁保持完整元数据",
    ])
    .unwrap();
    let seeded = std::time::Instant::now();
    server.state.storage.library_transaction(|transaction| {
        let mut models = transaction.prepare(
            "INSERT INTO library_models VALUES (?1, ?2, 'UploaderNeedle', ?3, 'InternalSynthetic', ?4,
             'FIGURA', 'default', '', 'PUBLIC', ?5, 'ACTIVE', 1, 1, ?6, ?6, 1)"
        ).unwrap();
        let mut revisions = transaction.prepare(
            "INSERT INTO library_revisions VALUES (?1, 1, ?2, 'synthetic', 1, ?3, 100,
             'NBT', 'avatar.json', 'SERVER_VERIFIED', 'synthetic.zip')"
        ).unwrap();
        let mut tags = transaction.prepare("INSERT INTO library_tags VALUES (?1, 'tagneedle', 'TagNeedle')").unwrap();
        for index in 0..10_000u128 {
            let id = Uuid::from_u128(100_000 + index).to_string();
            let name = format!("中文模型标题保留完整信息 {index:05}");
            models.execute(params![id, owner, name, authors, index % 3 == 0, index as i64 + 1]).unwrap();
            revisions.execute(params![id, owner, "a".repeat(64)]).unwrap();
            if index < 20 {
                tags.execute([id]).unwrap();
            }
        }
        Ok(())
    }).unwrap();
    println!(
        "M7 metadata seed: 10000 models, four long Chinese authors; {} ms",
        seeded.elapsed().as_millis()
    );
    let mut elapsed = Vec::new();
    let mut maximum_bytes = 0;
    for page in [1, 2, 625, 1249, 1250].into_iter().cycle().take(20) {
        let started = std::time::Instant::now();
        let response = server
            .request(
                Method::GET,
                &format!(
                    "/api/library/v1/models?q={}&sort=name&page_size=8&page={page}",
                    "InternalSynthetic"
                ),
                2,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = response.bytes().await.unwrap();
        elapsed.push(started.elapsed().as_micros());
        maximum_bytes = maximum_bytes.max(bytes.len());
        assert!(bytes.len() <= 65536);
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["total"], 10_000);
        assert_eq!(body["items"].as_array().unwrap().len(), 8);
        assert_eq!(body["has_next"], page < 1250);
        let first_name = body["items"][0]["display_name"].as_str().unwrap();
        assert!(first_name.ends_with(&format!("{:05}", (page - 1) * 8)));
    }
    elapsed.sort();
    for size in [12, 24, 64] {
        for page in [1, 2] {
            let response = server
                .request(
                    Method::GET,
                    &format!("/api/library/v1/models?sort=name&page_size={size}&page={page}"),
                    2,
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let bytes = response.bytes().await.unwrap();
            assert!(bytes.len() <= 65536);
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["items"].as_array().unwrap().len(), size as usize);
            assert_eq!(body["page_size"], size);
            assert_eq!(body["total"], 10_000);
            assert_eq!(body["has_next"], true);
            assert!(
                body["items"][0]["display_name"]
                    .as_str()
                    .unwrap()
                    .ends_with(&format!("{:05}", (page - 1) * size))
            );
        }
    }
    println!("M7 list HTTP: samples=20, rows=10000, page_size=8, p50={} us, p95={} us, max={} us, max_body={} bytes",
        elapsed[9], elapsed[18], elapsed[19], maximum_bytes);
    assert_eq!(
        server
            .request(Method::GET, "/api/library/v1/models?page_size=65", 2)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let tags = server
        .request(Method::GET, "/api/library/v1/models?q=TagNeedle&page_size=8", 2)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(tags["total"], 20, "search contract includes visible models' tags");
    let credentials = admin::issue(&server).await;
    let admin_page = server
        .request(Method::GET, "/api/library/v1/models?scope=admin&q=UploaderNeedle", 4)
        .header("x-omnimorph-origin", &server.base)
        .header("x-omnimorph-admin", credentials["access_token"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(admin_page["total"], 10_000, "administrator search includes the uploader");
    server
        .state
        .storage
        .library_read(|database| {
            let mut statement = database
                .prepare(
                    "EXPLAIN QUERY PLAN SELECT model_id FROM library_effective_models
             WHERE published = 1 AND moderation_status = 'ACTIVE' AND visibility = 'PUBLIC'
             ORDER BY updated_at DESC, model_id LIMIT 8",
                )
                .unwrap();
            let plan = statement
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            println!("M7 SQLite page query plan: {}", plan.join(" | "));
            assert!(plan.iter().any(|line| line.contains("INDEX")));
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn verified_wardrobe_copy_can_appeal_a_ban_from_another_upload_without_private_metadata() {
    let server = Server::start().await;
    let (model, bytes) = server.upload_model("PUBLIC").await;
    wardrobe::upload(&server, 21, &bytes).await;
    let credentials = admin::issue(&server).await;
    let response = server
        .request(Method::PUT, &format!("/api/library/v1/admin/models/{model}/block"), 4)
        .header("x-omnimorph-origin", &server.base)
        .header("x-omnimorph-admin", credentials["access_token"].as_str().unwrap())
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let targets = server
        .request(Method::GET, "/api/library/v1/appeal-targets", 21)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(targets["total"], 1);
    assert_eq!(targets["items"][0]["target_id"], model);
    assert_eq!(targets["items"][0]["name"], "avatar");
    let input = json!({"operation_id": library::random_id(), "target_kind": "LIBRARY",
        "target_id": model, "reason": "Synthetic appeal from a verified own wardrobe copy"});
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/appeals", 21)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/appeals", 2)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{model}"), 21)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}
