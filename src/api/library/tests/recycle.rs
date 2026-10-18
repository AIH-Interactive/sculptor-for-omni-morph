use super::*;

async fn deleted_wardrobe(server: &Server, player: u128, bytes: &[u8]) -> (String, u64) {
    wardrobe::upload(server, player, bytes).await;
    let owner = Uuid::from_u128(player);
    crate::service::resource::ensure_legacy_import(&server.state, owner, true)
        .await
        .unwrap();
    let avatar = server.state.storage.avatar(owner, "avatar", 0).unwrap();
    let operation = library::random_id();
    let ack = server
        .state
        .storage
        .delete(
            owner,
            &crate::protocol::v1::Delete {
                operation_id: operation.clone(),
                asset_id: avatar.asset_id,
                expected_revision: avatar.revision,
            },
        )
        .unwrap();
    (operation, ack.revision)
}

#[tokio::test]
async fn wardrobe_recycle_restores_bytes_permissions_and_durable_replay() {
    const PLAYER: u128 = 22_001;
    let server = Server::start_with_layout(true).await;
    let bytes = figura_package();
    wardrobe::upload(&server, PLAYER, &bytes).await;
    let owner = Uuid::from_u128(PLAYER);
    crate::service::resource::ensure_legacy_import(&server.state, owner, true)
        .await
        .unwrap();
    let credentials = admin::issue(&server).await;
    let token = credentials["access_token"].as_str().unwrap();
    let request = |method, path: &str| {
        server
            .request(method, path, 4)
            .header("x-omnimorph-origin", &server.base)
            .header("x-omnimorph-admin", token)
    };
    let avatar = server.state.storage.avatar(owner, "avatar", 0).unwrap();
    let equipped = server.state.storage.equipped(owner).unwrap();
    let input = json!({"operation_id": library::random_id(), "expected_equipment_revision": equipped.revision,
        "asset_id": avatar.asset_id, "asset_revision": avatar.revision, "show_equipment": false});
    assert_eq!(
        server
            .request(Method::PATCH, "/api/library/v1/wardrobe", PLAYER)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let equipped = server.state.storage.equipped(owner).unwrap();
    let block = json!({"operation_id": library::random_id(), "expected_equipment_revision": equipped.revision,
        "asset_id": avatar.asset_id, "asset_revision": avatar.revision});
    assert_eq!(
        request(
            Method::POST,
            &format!("/api/library/v1/admin/wardrobe/{owner}/block")
        )
        .json(&block)
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    let archive = library::random_id();
    let ack = server
        .state
        .storage
        .delete(
            owner,
            &crate::protocol::v1::Delete {
                operation_id: archive.clone(),
                asset_id: avatar.asset_id.clone(),
                expected_revision: avatar.revision,
            },
        )
        .unwrap();
    let list = "/api/library/v1/admin/recycle";
    assert_eq!(
        server
            .request(Method::GET, list, PLAYER)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let page: Value = request(Method::GET, list)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["operation_id"], archive);
    assert_eq!(page["items"][0]["can_restore"], true);
    let preview = format!("/api/library/v1/admin/wardrobe-preview/RECYCLE/{archive}?owner={owner}");
    assert_eq!(
        server
            .request(Method::GET, &preview, PLAYER)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let head = request(Method::HEAD, &preview).send().await.unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-length"], bytes.len().to_string());
    assert!(head.bytes().await.unwrap().is_empty());
    assert_eq!(
        request(Method::GET, &preview)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        bytes.as_slice()
    );
    let path = format!("/api/library/v1/admin/wardrobe-recycle/{owner}/{archive}/restore");
    let input =
        json!({"operation_id": library::random_id(), "expected_equipment_revision": ack.revision});
    assert_eq!(
        server
            .request(Method::POST, &path, PLAYER)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        request(Method::POST, &path)
            .json(&json!({"operation_id": library::random_id(),
        "expected_equipment_revision": 99999}))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let response = request(Method::POST, &path)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert_eq!(
        request(Method::POST, &path)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let own: Value = server
        .request(Method::GET, "/api/library/v1/wardrobe", PLAYER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(own["blocked"], true);
    assert_eq!(own["show_equipment"], false);
    assert_eq!(
        request(Method::GET, &preview)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let root = server._directory.path().join("data/avatars");
    let hash = faster_hex::hex_string(&crate::storage::sha256(&bytes));
    assert_eq!(
        std::fs::read(root.join("wardrobe").join(&hash)).unwrap(),
        bytes
    );
    assert!(!root.join("models").join(&hash).exists());
    assert!(
        !root
            .join("recycle/wardrobe")
            .join(owner.to_string())
            .join(&archive)
            .exists(),
        "restoration moves the original files out of the recycle directory"
    );
    let current: Value = request(Method::GET, "/api/library/v1/admin/wardrobe")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        current["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["owner"] == owner.to_string()),
        "the restored source returns to wardrobe management"
    );
    let reopened = Storage::open_avatars(
        &root,
        server._directory.path().join("data/metadata.sqlite3"),
    )
    .unwrap();
    let actor = crate::service::library_policy::Actor {
        uuid: Uuid::from_u128(4),
        admin_context: true,
        granted: false,
    };
    let restore =
        serde_json::from_value::<crate::storage::wardrobe_recycle::Restore>(input.clone()).unwrap();
    reopened
        .wardrobe_restore(actor, owner, Uuid::parse_str(&archive).unwrap(), &restore)
        .unwrap();
    assert!(reopened.equipped(owner).unwrap().primary.is_some());
    let page = reopened
        .library_recycle_list(
            actor,
            &crate::storage::wardrobe_recycle::RecycleQuery {
                page: 1,
                q: String::new(),
                model_type: String::new(),
                source: String::new(),
            },
        )
        .unwrap();
    assert_eq!(
        page["total"], 0,
        "restoration and archive consumption survive reopening storage"
    );
    drop(reopened);
    let second = library::random_id();
    server
        .state
        .storage
        .delete(
            owner,
            &crate::protocol::v1::Delete {
                operation_id: second.clone(),
                asset_id: avatar.asset_id,
                expected_revision: avatar.revision,
            },
        )
        .unwrap();
    let page: Value = request(Method::GET, list)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        page["total"], 1,
        "a previously restored archive must not reappear after a later deletion"
    );
    assert_eq!(page["items"][0]["operation_id"], second);
    assert_eq!(
        request(Method::POST, &path)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        server
            .state
            .storage
            .equipped(owner)
            .unwrap()
            .primary
            .is_none(),
        "old replay must not re-equip a later deletion"
    );
}

#[tokio::test]
async fn wardrobe_recycle_rejects_newer_uploads_and_damaged_archives() {
    const PLAYER: u128 = 22_002;
    let server = Server::start_with_layout(true).await;
    let bytes = figura_package();
    let owner = Uuid::from_u128(PLAYER);
    let (archive, _) = deleted_wardrobe(&server, PLAYER, &bytes).await;
    let credentials = admin::issue(&server).await;
    let token = credentials["access_token"].as_str().unwrap();
    let request = |method, path: &str| {
        server
            .request(method, path, 4)
            .header("x-omnimorph-origin", &server.base)
            .header("x-omnimorph-admin", token)
    };
    wardrobe::upload(&server, PLAYER, &wardrobe::repack(&bytes, true)).await;
    crate::service::resource::ensure_legacy_import(&server.state, owner, true)
        .await
        .unwrap();
    let newer = server.state.storage.equipped(owner).unwrap();
    let path = format!("/api/library/v1/admin/wardrobe-recycle/{owner}/{archive}/restore");
    assert_eq!(
        request(Method::POST, &path)
            .json(&json!({"operation_id": library::random_id(),
        "expected_equipment_revision": newer.revision}))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(server.state.storage.equipped(owner).unwrap(), newer);
    let page: Value = request(Method::GET, "/api/library/v1/admin/recycle")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["items"][0]["can_restore"], false);
    let (archive, revision) = deleted_wardrobe(&server, 22_003, &bytes).await;
    let owner = Uuid::from_u128(22_003);
    let file = server
        ._directory
        .path()
        .join("data/avatars/recycle/wardrobe")
        .join(owner.to_string())
        .join(&archive)
        .join(faster_hex::hex_string(&crate::storage::sha256(&bytes)));
    std::fs::write(file, b"damaged").unwrap();
    let path = format!("/api/library/v1/admin/wardrobe-recycle/{owner}/{archive}/restore");
    assert_eq!(
        request(Method::POST, &path)
            .json(&json!({"operation_id": library::random_id(),
        "expected_equipment_revision": revision}))
            .send()
            .await
            .unwrap()
            .status(),
        500
    );
    assert!(
        server
            .state
            .storage
            .equipped(owner)
            .unwrap()
            .primary
            .is_none()
    );
}

#[tokio::test]
async fn unified_recycle_hides_deleted_sources_and_moves_or_purges_only_their_files() {
    let server = Server::start_with_layout(true).await;
    let (live, bytes) = server.upload_model("PUBLIC").await;
    let (deleted, _) = server.upload_model("PUBLIC").await;
    delete_model(&server, &deleted, 2).await;
    let owner = Uuid::from_u128(23_001);
    let (archive, revision) = deleted_wardrobe(&server, 23_001, &bytes).await;
    wardrobe::upload(&server, 23_002, &bytes).await;
    crate::service::resource::ensure_legacy_import(&server.state, Uuid::from_u128(23_002), true)
        .await
        .unwrap();
    let credentials = admin::issue(&server).await;
    let token = credentials["access_token"].as_str().unwrap();
    let request = |method, path: &str| {
        server
            .request(method, path, 4)
            .header("x-omnimorph-origin", &server.base)
            .header("x-omnimorph-admin", token)
    };
    let list = "/api/library/v1/admin/recycle";
    assert_eq!(
        server
            .request(Method::GET, list, 1)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let page: Value = request(Method::GET, list)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["total"], 2);
    for (filters, expected) in [
        ("type=FIGURA", 2),
        ("type=YSM_PLAIN", 0),
        ("source=LIBRARY", 1),
        ("source=WARDROBE&type=FIGURA", 1),
        ("source=LIBRARY&type=YSM_ENCRYPTED", 0),
    ] {
        let filtered: Value = request(Method::GET, &format!("{list}?{filters}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(filtered["total"], expected, "filter {filters}");
        assert_eq!(
            filtered["items"].as_array().unwrap().len(),
            expected as usize
        );
    }
    assert!(
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "LIBRARY" && item["id"] == deleted)
    );
    assert!(
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "WARDROBE" && item["id"] == archive)
    );
    assert!(
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["status"] == "DELETED")
    );
    let second: Value = request(Method::GET, &format!("{list}?page=2"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["total"], 2);
    assert!(second["items"].as_array().unwrap().is_empty());
    let models: Value = request(Method::GET, "/api/library/v1/models?scope=admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["total"], 1);
    assert_eq!(models["items"][0]["model_id"], live);
    let wardrobe: Value = request(Method::GET, "/api/library/v1/admin/wardrobe")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !wardrobe["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["owner"] == owner.to_string())
    );
    let preview = format!("/api/library/v1/admin/wardrobe-preview/LIBRARY_RECYCLE/{deleted}");
    assert_eq!(
        server
            .request(Method::GET, &preview, 1)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        request(Method::GET, &preview)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        bytes.as_slice()
    );
    assert_eq!(
        admin_mutation(&server, &deleted, "restore", 3, token)
            .await
            .status(),
        200
    );
    let root = server._directory.path().join("data/avatars");
    let hash = faster_hex::hex_string(&crate::storage::sha256(&bytes));
    assert!(!root.join("recycle/library").join(&deleted).exists());
    assert!(root.join("models").join(&hash).is_file());
    let models: Value = request(Method::GET, "/api/library/v1/models?scope=admin")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["total"], 2);
    delete_model(&server, &deleted, 4).await;
    assert_eq!(
        admin_mutation(&server, &deleted, "purge", 5, token)
            .await
            .status(),
        200
    );
    assert!(
        root.join("models").join(&hash).is_file(),
        "shared live library bytes survive purge"
    );
    let path = format!("/api/library/v1/admin/wardrobe-recycle/{owner}/{archive}/purge");
    let input =
        json!({"operation_id": library::random_id(), "expected_equipment_revision": revision});
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        request(Method::POST, &path)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        request(Method::POST, &path)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        !root
            .join("recycle/wardrobe")
            .join(owner.to_string())
            .join(&archive)
            .exists()
    );
    assert!(
        root.join("wardrobe").join(&hash).is_file(),
        "shared live wardrobe bytes survive purge"
    );
    let path = format!("/api/library/v1/admin/wardrobe-recycle/{owner}/{archive}/restore");
    assert_eq!(
        request(Method::POST, &path)
            .json(&json!({"operation_id": library::random_id(),
        "expected_equipment_revision": revision}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let page: Value = request(Method::GET, list)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["total"], 0);
}

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
