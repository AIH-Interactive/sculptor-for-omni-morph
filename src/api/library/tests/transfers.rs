use super::*;

#[tokio::test]
async fn outfit_json_download_requires_its_own_receipt_and_preserves_policy() {
    let server = Server::start().await;
    let configuration =
        serde_json::to_value(crate::service::outfit::OutfitConfig::default()).unwrap();
    let response = server
        .request(Method::POST, "/api/library/v1/uploads/outfit", 1)
        .header("x-omnimorph-origin", &server.base)
        .json(&json!({"operation_id": library::random_id(), "metadata": {
            "resource_kind": "OUTFIT", "display_name": "JSON预设", "internal_name": "Preset",
            "authors": [], "category_id": "general", "description": "", "visibility": "PUBLIC",
            "requires_authorization": false}, "configuration": configuration}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let published: Value = response.json().await.unwrap();
    let id = published["model_id"].as_str().unwrap();
    let detail: Value = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(detail["model_type"].is_null());
    assert_eq!(detail["metadata_source"], "outfit.json");
    let preview = transfer(&server, id, "preview", 2).await;
    let response = server
        .request(Method::GET, preview["relative_path"].as_str().unwrap(), 2)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    let hash = response.headers()["x-omnimorph-sha256"]
        .to_str()
        .unwrap()
        .to_owned();
    let bytes = response.bytes().await.unwrap();
    assert_eq!(detail["byte_length"], bytes.len());
    assert_eq!(
        hash,
        faster_hex::hex_string(&crate::storage::sha256(&bytes))
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        configuration
    );
    let completion = json!({"operation_id": library::random_id(), "raw_sha256": hash});
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("{}/complete", preview["relative_path"].as_str().unwrap()),
                2
            )
            .json(&completion)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(
        server
            .request(Method::PUT, &format!("/api/library/v1/collection/{id}"), 2)
            .json(&json!({"transfer_id": preview["transfer_id"]}))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let download = transfer(&server, id, "download", 2).await;
    let complete_path = format!("{}/complete", download["relative_path"].as_str().unwrap());
    assert_eq!(
        server
            .request(Method::POST, &complete_path, 2)
            .json(&completion)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert!(!server
        .request(Method::GET, download["relative_path"].as_str().unwrap(), 3)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let response = server
        .request(Method::GET, download["relative_path"].as_str().unwrap(), 2)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap(), bytes);
    let wrong = json!({"operation_id": library::random_id(), "raw_sha256": "0".repeat(64)});
    assert_eq!(
        server
            .request(Method::POST, &complete_path, 2)
            .json(&wrong)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::POST, &complete_path, 2)
                .json(&completion)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            server
                .request(Method::PUT, &format!("/api/library/v1/collection/{id}"), 2)
                .json(&json!({"transfer_id": download["transfer_id"]}))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let detail: Value = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["downloads"], 1);
    let stale = transfer(&server, id, "download", 2).await;
    let response = server.request(Method::PATCH, &format!("/api/library/v1/models/{id}"), 1)
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 2, "visibility": "PRIVATE"}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert!(!server
        .request(Method::GET, stale["relative_path"].as_str().unwrap(), 2)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(!server
        .request(
            Method::POST,
            &format!("{}/complete", stale["relative_path"].as_str().unwrap()),
            2
        )
        .json(&json!({"operation_id": library::random_id(), "raw_sha256": hash}))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

async fn transfer(server: &Server, id: &str, purpose: &str, player: u128) -> Value {
    let response = server
        .request(
            Method::POST,
            &format!("/api/library/v1/models/{id}/transfers"),
            player,
        )
        .json(
            &json!({"operation_id": library::random_id(), "purpose": purpose, "model_revision": 1}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    response.json().await.unwrap()
}

#[tokio::test]
async fn preview_is_not_counted_and_download_needs_complete_stream_and_digest() {
    let server = Server::start().await;
    let (id, bytes) = server.upload_model("PUBLIC").await;
    let preview = transfer(&server, &id, "preview", 2).await;
    let response = server
        .request(Method::GET, preview["relative_path"].as_str().unwrap(), 2)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
    let completion = json!({"operation_id": library::random_id(), "raw_sha256": faster_hex::hex_string(&crate::storage::sha256(&bytes))});
    let path = format!("{}/complete", preview["relative_path"].as_str().unwrap());
    assert_eq!(
        server
            .request(Method::POST, &path, 2)
            .json(&completion)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let download = transfer(&server, &id, "download", 2).await;
    let path = format!("{}/complete", download["relative_path"].as_str().unwrap());
    assert_eq!(
        server
            .request(Method::POST, &path, 2)
            .json(&completion)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(
        server
            .request(Method::GET, download["relative_path"].as_str().unwrap(), 3)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let response = server
        .request(Method::GET, download["relative_path"].as_str().unwrap(), 2)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::POST, &path, 2)
                .json(&completion)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let second = transfer(&server, &id, "download", 2).await;
    server
        .request(Method::GET, second["relative_path"].as_str().unwrap(), 2)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let path = format!("{}/complete", second["relative_path"].as_str().unwrap());
    let another =
        json!({"operation_id": library::random_id(), "raw_sha256": completion["raw_sha256"]});
    assert_eq!(
        server
            .request(Method::POST, &path, 2)
            .json(&another)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let detail = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(detail["downloads"], 1);
}

#[tokio::test]
async fn existing_transfers_recheck_policy_revision_and_expiry_and_admission_is_bounded() {
    let server = Server::start().await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let external = transfer(&server, &id, "preview", 2).await;
    let owner = transfer(&server, &id, "download", 1).await;
    server.state.storage.library_transaction(|transaction| {
        transaction.execute("UPDATE library_models SET visibility = 'PRIVATE', policy_revision = policy_revision + 1 WHERE model_id = ?1", [&id]).map_err(library::sql_error)?;
        Ok(())
    }).unwrap();
    assert_eq!(
        server
            .request(Method::GET, external["relative_path"].as_str().unwrap(), 2)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .request(Method::GET, owner["relative_path"].as_str().unwrap(), 1)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let current = transfer(&server, &id, "preview", 1).await;
    let response = server.request(Method::POST, &format!("/api/library/v1/models/{id}/transfers"), 1)
        .json(&json!({"operation_id": library::random_id(), "purpose": "preview", "model_revision": 1})).send().await.unwrap();
    assert_eq!(response.status(), 429);
    server
        .state
        .storage
        .library_transaction(|transaction| {
            transaction
                .execute(
                    "UPDATE library_download_receipts SET expires_at = 0 WHERE transfer_id = ?1",
                    [current["transfer_id"].as_str().unwrap()],
                )
                .map_err(library::sql_error)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        server
            .request(Method::GET, current["relative_path"].as_str().unwrap(), 1)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}
