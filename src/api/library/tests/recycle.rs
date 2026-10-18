use super::*;

async fn admin_mutation(
    server: &Server,
    id: &str,
    action: &str,
    revision: u64,
    token: &str,
) -> reqwest::Response {
    server
        .request(
            Method::POST,
            &format!("/api/library/v1/admin/models/{id}/{action}"),
            4,
        )
        .header("x-omnimorph-origin", &server.base)
        .header("x-omnimorph-admin", token)
        .json(&json!({"operation_id": library::random_id(), "expected_revision": revision}))
        .send()
        .await
        .unwrap()
}

async fn delete_model(server: &Server, id: &str, revision: u64) -> Value {
    let response = server
        .request(Method::DELETE, &format!("/api/library/v1/models/{id}"), 1)
        .json(&json!({"operation_id": library::random_id(), "expected_revision": revision}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

#[tokio::test]
async fn wardrobe_files_are_separate_and_never_become_catalog_entries() {
    let server = Server::start_with_layout(true).await;
    let bytes = figura_package();
    wardrobe::upload(&server, 2101, &bytes).await;
    crate::service::resource::ensure_legacy_import(&server.state, Uuid::from_u128(2101), true)
        .await
        .unwrap();
    let root = server._directory.path().join("data/avatars");
    let hash = faster_hex::hex_string(&crate::storage::sha256(&bytes));
    assert_eq!(
        std::fs::read(root.join("wardrobe").join(&hash)).unwrap(),
        bytes
    );
    assert_eq!(std::fs::read_dir(root.join("models")).unwrap().count(), 0);
    let catalog = server
        .request(Method::GET, "/api/library/v1/models", 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(catalog["total"], 0);

    let (id, _) = server.upload_model("PUBLIC").await;
    assert!(root.join("models").join(&hash).is_file());
    let deleted = delete_model(&server, &id, 2).await;
    assert_eq!(deleted["policy_revision"], 3);
    assert!(!root.join("models").join(&hash).exists());
    assert!(
        root.join("wardrobe").join(&hash).is_file(),
        "same wardrobe content remains independent"
    );
    assert_eq!(
        std::fs::read(root.join("recycle/library").join(&id).join(&hash)).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn recycle_restore_and_purge_require_admin_cas_and_preserve_shared_models() {
    let server = Server::start_with_layout(true).await;
    let (id, bytes) = server.upload_model("PUBLIC").await;
    let (other, _) = server.upload_model("PUBLIC").await;
    let credentials = admin::issue(&server).await;
    let token = credentials["access_token"].as_str().unwrap();
    let root = server._directory.path().join("data/avatars");
    let hash = faster_hex::hex_string(&crate::storage::sha256(&bytes));
    delete_model(&server, &id, 2).await;
    assert!(root.join("models").join(&hash).is_file());
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("/api/library/v1/admin/models/{id}/restore"),
                1
            )
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 3}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        admin_mutation(&server, &id, "restore", 1, token)
            .await
            .status(),
        409
    );
    assert_eq!(
        admin_mutation(&server, &id, "restore", 3, token)
            .await
            .status(),
        200
    );
    assert!(!root.join("recycle/library").join(&id).exists());
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{id}"), 1)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    delete_model(&server, &id, 4).await;
    assert_eq!(
        admin_mutation(&server, &id, "purge", 5, token)
            .await
            .status(),
        200
    );
    assert!(!root.join("recycle/library").join(&id).exists());
    assert!(
        root.join("models").join(&hash).is_file(),
        "shared live library model must survive purge"
    );
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{other}"), 1)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        admin_mutation(&server, &id, "restore", 6, token)
            .await
            .status(),
        404
    );
    delete_model(&server, &other, 2).await;
    assert!(!root.join("models").join(&hash).exists());
    assert_eq!(
        admin_mutation(&server, &other, "purge", 3, token)
            .await
            .status(),
        200
    );
    assert!(!root.join("recycle/library").join(&other).exists());
}

#[tokio::test]
async fn wardrobe_delete_keeps_payload_and_protobuf_recovery_record() {
    let server = Server::start_with_layout(true).await;
    let bytes = figura_package();
    wardrobe::upload(&server, 2102, &bytes).await;
    crate::service::resource::ensure_legacy_import(&server.state, Uuid::from_u128(2102), true)
        .await
        .unwrap();
    let avatar = server
        .state
        .storage
        .avatar(Uuid::from_u128(2102), "avatar", 0)
        .unwrap();
    let operation = library::random_id();
    server
        .state
        .storage
        .delete(
            Uuid::from_u128(2102),
            &crate::protocol::v1::Delete {
                operation_id: operation.clone(),
                asset_id: avatar.asset_id,
                expected_revision: avatar.revision,
            },
        )
        .unwrap();
    let root = server._directory.path().join("data/avatars");
    let hash = faster_hex::hex_string(&crate::storage::sha256(&bytes));
    let archive = root
        .join("recycle/wardrobe")
        .join(Uuid::from_u128(2102).to_string())
        .join(operation);
    assert_eq!(std::fs::read(archive.join(&hash)).unwrap(), bytes);
    assert!(archive.join("revision-1.pb").is_file());
    assert!(archive.join("metadata.json").is_file());
    assert!(!root.join("wardrobe").join(&hash).exists());
}

#[tokio::test]
async fn restored_blocked_model_stays_private_and_old_restore_replay_keeps_new_archive() {
    let server = Server::start_with_layout(true).await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let credentials = admin::issue(&server).await;
    let token = credentials["access_token"].as_str().unwrap();
    let request = |method, path: &str| {
        server
            .request(method, path, 4)
            .header("x-omnimorph-origin", &server.base)
            .header("x-omnimorph-admin", token)
    };
    assert_eq!(
        request(
            Method::PUT,
            &format!("/api/library/v1/admin/models/{id}/block")
        )
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 2}))
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    assert_eq!(
        request(Method::DELETE, &format!("/api/library/v1/models/{id}"))
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 3}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let restore_input = json!({"operation_id": library::random_id(), "expected_revision": 4});
    let restore_path = format!("/api/library/v1/admin/models/{id}/restore");
    assert_eq!(
        request(Method::POST, &restore_path)
            .json(&restore_input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let detail = request(Method::GET, &format!("/api/library/v1/models/{id}"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(detail["moderation_status"], "BLOCKED");
    assert_eq!(detail["visibility"], "PRIVATE");
    assert_eq!(
        request(Method::DELETE, &format!("/api/library/v1/models/{id}"))
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 5}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        request(Method::POST, &restore_path)
            .json(&restore_input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(server
        ._directory
        .path()
        .join("data/avatars/recycle/library")
        .join(id)
        .is_dir());
}

#[tokio::test]
async fn production_restart_archives_preexisting_deleted_library_payloads() {
    let server = Server::start_with_layout(true).await;
    let (id, bytes) = server.upload_model("PUBLIC").await;
    server
        .state
        .storage
        .library_transaction(|transaction| {
            transaction
                .execute(
                    "UPDATE library_models SET moderation_status = 'DELETED' WHERE model_id = ?1",
                    [&id],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    let data = server._directory.path().join("data");
    let reopened =
        Storage::open_avatars(data.join("avatars"), data.join("metadata.sqlite3")).unwrap();
    let hash = faster_hex::hex_string(&crate::storage::sha256(&bytes));
    assert!(!data.join("avatars/models").join(&hash).exists());
    assert_eq!(
        std::fs::read(data.join("avatars/recycle/library").join(&id).join(hash)).unwrap(),
        bytes
    );
    assert!(reopened
        .library_recover(
            crate::service::library_policy::Actor {
                uuid: Uuid::from_u128(4),
                admin_context: true,
                granted: false,
            },
            &id,
            &serde_json::from_value(
                json!({"operation_id": library::random_id(), "expected_revision": 2})
            )
            .unwrap(),
            false
        )
        .is_ok());
}
