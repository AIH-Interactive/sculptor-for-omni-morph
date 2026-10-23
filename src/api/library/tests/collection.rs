use super::*;

async fn members(server: &Server, player: u128) -> Value {
    let response = server
        .request(Method::GET, "/api/library/v1/collection", player)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

#[tokio::test]
async fn removing_members_is_account_scoped_repeatable_and_keeps_the_original_model() {
    let mut server = Server::start().await;
    let (id, _) = server.upload_model("PRIVATE").await;
    let database =
        rusqlite::Connection::open(server._directory.path().join("metadata.sqlite3")).unwrap();
    // An old member may no longer have permission to read the original private model.
    database.execute(
        "INSERT INTO library_collection(player_uuid, model_id, joined_at, join_source, in_outfit)
         SELECT ?1, model_id, joined_at, 'DOWNLOAD', 1 FROM library_collection WHERE model_id = ?2",
        rusqlite::params![Uuid::from_u128(2).to_string(), id],
    ).unwrap();
    let path = format!("/api/library/v1/collection/{id}");
    assert_eq!(
        server
            .request(Method::DELETE, &path, 3)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(members(&server, 2).await["total"], 1);
    assert!(members(&server, 2).await["items"][0]["model"].is_null());
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::DELETE, &path, 2)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(members(&server, 2).await["total"], 0);
    assert_eq!(members(&server, 1).await["total"], 1);
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{id}"), 1)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        server
            .request(Method::DELETE, "/api/library/v1/collection/not-a-uuid", 2)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    drop(database);
    super::lifecycle::restart(&mut server).await;
    assert_eq!(members(&server, 2).await["total"], 0);
    assert_eq!(members(&server, 1).await["total"], 1);
}

#[tokio::test]
async fn published_uploads_are_members_but_outfits_and_other_accounts_are_independent() {
    let mut server = Server::start().await;
    let (id, _) = server.upload_model("PRIVATE").await;
    let collection = members(&server, 1).await;
    assert_eq!(collection["total"], 1);
    assert_eq!(collection["items"][0]["model_id"], id);
    assert_eq!(collection["items"][0]["join_source"], "UPLOAD");
    assert_eq!(collection["items"][0]["in_outfit"], false);
    assert_eq!(members(&server, 2).await["total"], 0);
    let path = format!("/api/library/v1/collection/{id}/outfit");
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::PUT, &path, 1)
                .json(&json!({"in_outfit": true}))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(members(&server, 1).await["items"][0]["in_outfit"], true);
    super::lifecycle::restart(&mut server).await;
    assert_eq!(members(&server, 1).await["items"][0]["in_outfit"], true);
    assert_ne!(
        server
            .request(Method::PUT, &path, 2)
            .json(&json!({"in_outfit": true}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(members(&server, 2).await["total"], 0);
}

#[tokio::test]
async fn joining_requires_own_completed_formal_transfer_and_does_not_equip() {
    let server = Server::start().await;
    let (id, bytes) = server.upload_model("PUBLIC").await;
    let response = server.request(Method::POST, &format!("/api/library/v1/models/{id}/transfers"), 2)
        .json(&json!({"operation_id": library::random_id(), "purpose": "download", "model_revision": 1}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    let transfer: Value = response.json().await.unwrap();
    let input = json!({"transfer_id": transfer["transfer_id"]});
    let join = format!("/api/library/v1/collection/{id}");
    assert_eq!(
        server
            .request(Method::PUT, &join, 2)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let relative = transfer["relative_path"].as_str().unwrap();
    assert_eq!(
        server
            .request(Method::GET, relative, 2)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        bytes
    );
    assert_eq!(
        server
            .request(Method::POST, &format!("{relative}/complete"), 2)
            .json(&json!({"operation_id": library::random_id(),
            "raw_sha256": faster_hex::hex_string(&crate::storage::sha256(&bytes))}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        server
            .request(Method::PUT, &join, 3)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::PUT, &join, 2)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(members(&server, 2).await["total"], 1);
    let session: Value = server
        .request(Method::GET, "/api/library/v1/session", 2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(session["equipment_model_id"].is_null());
    assert_eq!(members(&server, 2).await["items"][0]["in_outfit"], false);
    assert_eq!(server.request(Method::PATCH, &format!("/api/library/v1/models/{id}"), 1)
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 2, "visibility": "PRIVATE"}))
        .send().await.unwrap().status(), 200);
    let hidden = members(&server, 2).await;
    assert_eq!(hidden["items"][0]["model_id"], id);
    assert!(hidden["items"][0]["model"].is_null());
    assert_eq!(
        server
            .request(Method::PUT, &format!("{join}/outfit"), 2)
            .json(&json!({"in_outfit": true}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .request(Method::PUT, &format!("{join}/outfit"), 2)
            .json(&json!({"in_outfit": false}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn migration_backfills_only_published_active_submissions() {
    let mut server = Server::start().await;
    let (active, _) = server.upload_model("PRIVATE").await;
    let (deleted, _) = server.upload_model("PUBLIC").await;
    let database =
        rusqlite::Connection::open(server._directory.path().join("metadata.sqlite3")).unwrap();
    crate::storage::library::tests::restore_legacy_download_receipts(&database);
    database
        .execute(
            "UPDATE library_models SET moderation_status = 'DELETED' WHERE model_id = ?1",
            [&deleted],
        )
        .unwrap();
    database
        .execute_batch("DROP TABLE library_collection; PRAGMA user_version = 5;")
        .unwrap();
    drop(database);
    super::lifecycle::restart(&mut server).await;
    let collection = members(&server, 1).await;
    assert_eq!(collection["total"], 1);
    assert_eq!(collection["items"][0]["model_id"], active);
    super::lifecycle::restart(&mut server).await;
    assert_eq!(members(&server, 1).await["total"], 1);
}
