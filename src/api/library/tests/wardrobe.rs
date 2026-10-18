use super::*;
use std::io::{Read, Write};

pub(super) fn repack(bytes: &[u8], change_name: bool) -> Vec<u8> {
    let mut nbt = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut nbt)
        .unwrap();
    if change_name {
        let name = nbt
            .windows(8)
            .position(|value| value == b"Internal")
            .unwrap();
        nbt[name..name + 8].copy_from_slice(b"OtherOne");
    }
    let mut encoder = flate2::GzBuilder::new()
        .mtime(12345)
        .write(Vec::new(), flate2::Compression::default());
    encoder.write_all(&nbt).unwrap();
    encoder.finish().unwrap()
}

pub(super) async fn upload(server: &Server, player: u128, bytes: &[u8]) {
    let uuid = Uuid::from_u128(player);
    server
        .state
        .user_manager
        .insert(
            uuid,
            format!("synthetic-token-{player}"),
            Userinfo {
                uuid,
                nickname: format!("Synthetic{player}"),
                ..Default::default()
            },
        )
        .unwrap();
    let response = server
        .client
        .put(format!("{}/api/avatar", server.base))
        .header("token", format!("synthetic-token-{player}"))
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
}

async fn avatar(server: &Server, owner: u128, observer: u128) -> reqwest::Response {
    server
        .client
        .get(format!(
            "{}/api/{}/avatar",
            server.base,
            Uuid::from_u128(owner)
        ))
        .header("token", format!("synthetic-token-{observer}"))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn retryable_inspection_limit_does_not_turn_commit_retry_into_not_found() {
    use crate::protocol::v1::{AvatarDescriptor, CommitUpload, ErrorCode, UploadRequest};

    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    let (bytes, _) = crate::service::package::tests::fixture(false, false);
    let mut nbt = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_slice())
        .read_to_end(&mut nbt)
        .unwrap();
    nbt.pop(); // Append a valid NBT list at the root that exceeds the inspection list budget.
    nbt.push(9);
    nbt.extend_from_slice(&4u16.to_be_bytes());
    nbt.extend_from_slice(b"list");
    nbt.push(1);
    nbt.extend_from_slice(&4097i32.to_be_bytes());
    nbt.extend_from_slice(&[0; 4097]);
    nbt.push(0);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&nbt).unwrap();
    let bytes = encoder.finish().unwrap();
    let operation = crate::service::resource::new_operation_id();
    let request = UploadRequest {
        operation_id: operation.clone(),
        resource: Some(AvatarDescriptor {
            asset_id: "avatar".into(),
            model_type: 2,
            distribution_format: 2,
            raw_sha256: crate::storage::sha256(&bytes),
            raw_length: bytes.len() as u64,
            content_sha256: crate::service::package::tests::fixture(false, false).1,
            schema_sha256: vec![1; 32],
            required_capabilities: vec![2],
            ..Default::default()
        }),
    };
    crate::service::resource::grant_upload(&server.state, owner, &request)
        .await
        .unwrap();
    let operation_uuid = Uuid::parse_str(&operation).unwrap();
    let mut writer =
        crate::service::resource::begin_upload(&server.state, owner, operation_uuid).unwrap();
    crate::service::resource::receive_upload(
        &mut writer,
        axum::body::Body::from(bytes.clone()),
        || Ok(()),
    )
    .await
    .unwrap();
    drop(writer);
    let commit = CommitUpload {
        operation_id: operation,
        raw_sha256: crate::storage::sha256(&bytes),
        raw_length: bytes.len() as u64,
    };
    for attempt in 1..=2 {
        assert_eq!(
            crate::service::resource::commit_upload(&server.state, owner, &commit).await,
            Err(ErrorCode::LimitExceeded),
            "retryable inspection failure must retain the upload on commit attempt {attempt}"
        );
    }
    let path = server.state.storage.upload_path(owner, operation_uuid);
    assert!(
        path.exists(),
        "retryable failure preserves the uploaded bytes"
    );
    assert!(server
        .state
        .storage
        .equipped(owner)
        .unwrap()
        .primary
        .is_none());
    crate::service::resource::cancel_upload(&server.state, owner, operation_uuid).unwrap();
    assert!(
        !path.exists(),
        "explicit cancellation still releases the retained upload"
    );
}

#[tokio::test]
async fn wardrobe_upload_survives_first_self_refresh_without_a_temporary_model() {
    let server = Server::start().await;
    let packages = [
        figura_package(),
        crate::service::package::tests::fixture(false, false).0,
        crate::service::package::tests::fixture(true, false).0,
    ];
    for (index, bytes) in packages.into_iter().enumerate() {
        // Legacy HTTP fixtures share the process-wide avatar directory.
        let player = 21_002 + index as u128;
        upload(&server, player, &bytes).await;
        let owner = Uuid::from_u128(player);
        assert!(!crate::service::resource::legacy_read_path(owner, true).exists());
        let equipped = server.state.storage.equipped(owner).unwrap();
        assert!(equipped.primary.is_some(), "upload persisted the equipment");

        for refresh in 1..=3 {
            let response = server
                .client
                .get(format!("{}/api/{owner}", server.base))
                .header("token", format!("synthetic-token-{player}"))
                .header("x-omnimorph-models", "ysm")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let profile = response.json::<Value>().await.unwrap();
            assert_eq!(
                profile["equipped"].as_array().unwrap().len(),
                1,
                "model type {} self refresh {refresh} must retain the uploaded equipment",
                index + 1
            );
            assert!(
                !server.state.user_manager.request_temp_state(owner, false),
                "a missing temporary model must not consume the one-shot preview state"
            );
            let response = server
                .client
                .get(format!("{}/api/{owner}/avatar", server.base))
                .header("token", format!("synthetic-token-{player}"))
                .header("x-omnimorph-models", "ysm")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_slice());
        }

        tokio::fs::remove_file(crate::service::resource::legacy_path(owner, false))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn administrator_preview_is_source_bound_and_does_not_expose_blocked_wardrobe_to_players() {
    // Legacy HTTP fixtures share the process-wide avatar directory; reserve this player's files.
    const PLAYER: u128 = 21_001;
    let server = Server::start().await;
    let bytes = figura_package();
    upload(&server, PLAYER, &bytes).await;
    let credentials = super::admin::issue(&server).await;
    let request = |method, path: &str| {
        server
            .request(method, path, 4)
            .header("x-omnimorph-origin", &server.base)
            .header(
                "x-omnimorph-admin",
                credentials["access_token"].as_str().unwrap(),
            )
    };
    let page = request(Method::GET, "/api/library/v1/admin/wardrobe?page=1")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let item = &page["items"][0];
    let owner = Uuid::from_u128(PLAYER);
    let path = format!("/api/library/v1/admin/wardrobe-preview/CURRENT/{owner}?asset_id={}&asset_revision={}&equipment_revision={}",
        item["asset_id"].as_str().unwrap(), item["asset_revision"], item["equipment_revision"]);
    assert_eq!(
        server
            .request(Method::GET, &path, PLAYER)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        request(Method::GET, &path)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        bytes.as_slice()
    );
    let head = request(Method::HEAD, &path).send().await.unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["x-omnimorph-preview-cache-seconds"], "3600");
    assert_eq!(
        head.headers()["x-omnimorph-sha256"],
        faster_hex::hex_string(&crate::storage::sha256(&bytes))
    );
    assert!(head.bytes().await.unwrap().is_empty());
    server
        .state
        .config
        .write()
        .await
        .model_library
        .preview_cache_seconds = 15;
    let capabilities = request(Method::GET, "/api/library/v1/capabilities")
        .send()
        .await
        .unwrap();
    assert_eq!(
        capabilities.headers()["x-omnimorph-preview-cache-seconds"],
        "15"
    );
    assert_eq!(
        capabilities.json::<Value>().await.unwrap()["preview_cache_seconds"],
        15
    );
    let stale = path.replace("equipment_revision=1", "equipment_revision=999999");
    assert_ne!(stale, path);
    assert_eq!(
        request(Method::GET, &stale).send().await.unwrap().status(),
        409
    );
    let operation = library::random_id();
    let input = json!({"operation_id": operation, "expected_equipment_revision": item["equipment_revision"],
        "asset_id": item["asset_id"], "asset_revision": item["asset_revision"]});
    assert_eq!(
        request(
            Method::POST,
            &format!("/api/library/v1/admin/wardrobe/{owner}/block")
        )
        .json(&input)
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    assert_eq!(avatar(&server, PLAYER, 4).await.status(), 404);
    let blocked = format!("/api/library/v1/admin/wardrobe-preview/BLOCK/{operation}");
    assert_eq!(
        server
            .request(Method::GET, &blocked, PLAYER)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let response = request(Method::GET, &blocked).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers()["cache-control"]
        .to_str()
        .unwrap()
        .contains("no-store"));
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_slice());
    upload(&server, PLAYER, &repack(&bytes, true)).await;
    assert_eq!(
        request(Method::GET, &path).send().await.unwrap().status(),
        409
    );
    assert_eq!(
        request(Method::GET, &blocked)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        bytes.as_slice()
    );
    let missing = format!(
        "/api/library/v1/admin/wardrobe-preview/BLOCK/{}",
        library::random_id()
    );
    assert_eq!(
        request(Method::GET, &missing)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn admin_manages_only_current_wardrobe_with_cas_reset_and_verified_self_only_ban() {
    let server = Server::start().await;
    let bytes = figura_package();
    upload(&server, 21, &bytes).await;
    upload(&server, 22, &bytes).await;
    let credentials = super::admin::issue(&server).await;
    let request = |method, path: &str| server.request(method, path, 4)
        .header("x-omnimorph-origin", &server.base)
        .header("x-omnimorph-admin", credentials["access_token"].as_str().unwrap());
    let list = "/api/library/v1/admin/wardrobe?q=Synthetic21&page=1";
    assert_eq!(server.request(Method::GET, list, 2).send().await.unwrap().status(), 403);
    let page = request(Method::GET, list).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(page["total"], 1);
    let item = &page["items"][0];
    assert_eq!(item["owner"], Uuid::from_u128(21).to_string());
    let block = format!("/api/library/v1/admin/wardrobe/{}/block", Uuid::from_u128(21));
    let input = json!({"operation_id": library::random_id(), "expected_equipment_revision": item["equipment_revision"],
        "asset_id": item["asset_id"], "asset_revision": item["asset_revision"]});
    assert_eq!(server.request(Method::POST, &block, 2).json(&input).send().await.unwrap().status(), 403);
    let mut stale = input.clone();
    stale["expected_equipment_revision"] = json!(999);
    assert_eq!(request(Method::POST, &block).json(&stale).send().await.unwrap().status(), 409);
    for _ in 0..2 {
        let response = request(Method::POST, &block).json(&input).send().await.unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    }
    assert_eq!(avatar(&server, 21, 21).await.status(), 200);
    assert_eq!(avatar(&server, 21, 22).await.status(), 404);
    assert_eq!(avatar(&server, 22, 21).await.status(), 404);
    let reset = format!("/api/library/v1/admin/wardrobe/{}/reset", Uuid::from_u128(21));
    let mut reset_input = input.clone();
    reset_input["operation_id"] = json!(library::random_id());
    for _ in 0..2 {
        let response = request(Method::POST, &reset).json(&reset_input).send().await.unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    }
    assert!(server.state.storage.equipped(Uuid::from_u128(21)).unwrap().primary.is_none());
    let after = request(Method::GET, list).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(after["total"], 0);
    // A stale snapshot cannot reset the replacement upload.
    upload(&server, 21, &repack(&bytes, true)).await;
    reset_input["operation_id"] = json!(library::random_id());
    assert_eq!(request(Method::POST, &reset).json(&reset_input).send().await.unwrap().status(), 409);
    assert!(server.state.storage.equipped(Uuid::from_u128(21)).unwrap().primary.is_some());
    let reopened = Storage::open(server._directory.path()).unwrap();
    assert!(reopened.library_hash_blocked("RAW_SHA256", &faster_hex::hex_string(&crate::storage::sha256(&bytes))).unwrap());
}

#[tokio::test]
async fn encrypted_hash_ban_covers_historical_internal_v1_and_temporary_uploads() {
    let server = Server::start().await;
    let id = super::subscriptions::encrypted_model(&server).await;
    let (bytes, _) = crate::service::package::tests::fixture(true, false);
    let mut source_nbt = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_slice())
        .read_to_end(&mut source_nbt)
        .unwrap();
    let mut changed_nbt = source_nbt.clone();
    assert_eq!(&changed_nbt[..3], &[10, 0, 0]);
    // A different legal NBT root name changes both wrapper identities, while the
    // embedded encrypted source stays byte-for-byte identical. Only its verified
    // ciphertext identity can match this copy to the catalog ban.
    changed_nbt.splice(1..3, [0, 1, b'x']);
    assert_ne!(
        crate::storage::sha256(&source_nbt),
        crate::storage::sha256(&changed_nbt)
    );
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&changed_nbt).unwrap();
    let copy = encoder.finish().unwrap();
    assert_ne!(
        crate::storage::sha256(&bytes),
        crate::storage::sha256(&copy)
    );
    server.state.config.write().await.token = Some("synthetic-v1-admin".into());
    for player in [811, 812, 813] {
        let uuid = Uuid::from_u128(player);
        server
            .state
            .user_manager
            .insert(
                uuid,
                format!("synthetic-token-{player}"),
                Userinfo {
                    uuid,
                    nickname: format!("Synthetic{player}"),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let read = |owner, viewer| {
        server
            .client
            .get(format!(
                "{}/api/{}/avatar",
                server.base,
                Uuid::from_u128(owner)
            ))
            .header("token", format!("synthetic-token-{viewer}"))
            .header("x-omnimorph-models", "ysm")
    };
    let internal_path = format!("/internal/{}/avatar", Uuid::from_u128(811));
    assert_eq!(
        server
            .client
            .put(format!("{}{internal_path}", server.base))
            .header("host", "lambda")
            .body(copy.clone())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(read(811, 3).send().await.unwrap().status(), 200);
    // Simulate an imported immutable blob from before the M3 identity index existed.
    server
        .state
        .storage
        .library_transaction(|transaction| {
            transaction
                .execute("DELETE FROM library_verified_blobs", [])
                .map_err(library::sql_error)?;
            Ok(())
        })
        .unwrap();
    let moderator = crate::service::library_policy::Actor {
        uuid: Uuid::from_u128(4),
        admin_context: true,
        granted: false,
    };
    let block = serde_json::from_value(json!({
        "operation_id": library::random_id(), "expected_revision": 2,
        "reason": "synthetic encrypted hash ban",
    }))
    .unwrap();
    server
        .state
        .storage
        .library_block(moderator, &id, &block, true)
        .unwrap();
    assert_eq!(read(811, 3).send().await.unwrap().status(), 404);
    let response = read(811, 811).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), copy.as_slice());

    let v1_path = format!("/api/v1/avatar/{}", Uuid::from_u128(812));
    assert_eq!(
        server
            .client
            .put(format!("{}{v1_path}", server.base))
            .header("token", "synthetic-v1-admin")
            .body(copy.clone())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(read(812, 3).send().await.unwrap().status(), 404);
    assert_eq!(read(812, 812).send().await.unwrap().status(), 200);

    let temp_path = format!("/internal/{}/temp", Uuid::from_u128(813));
    assert_eq!(
        server
            .client
            .put(format!("{}{temp_path}", server.base))
            .header("host", "lambda")
            .body(copy.clone())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(read(813, 3).send().await.unwrap().status(), 404);
    let response = read(813, 813).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), copy.as_slice());
    assert_eq!(read(813, 813).send().await.unwrap().status(), 404);
    for player in [811, 812] {
        tokio::fs::remove_file(crate::service::resource::legacy_path(
            Uuid::from_u128(player),
            false,
        ))
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn verified_nbt_bans_cover_repacked_and_new_wardrobe_uploads_across_accounts() {
    let server = Server::start().await;
    let (id, bytes) = server.upload_model("PUBLIC").await;
    let copy = repack(&bytes, false);
    assert_ne!(
        crate::storage::sha256(&copy),
        crate::storage::sha256(&bytes)
    );
    upload(&server, 111, &copy).await;
    let response = avatar(&server, 111, 3).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), copy.as_slice());
    let actor = crate::service::library_policy::Actor {
        uuid: Uuid::from_u128(4),
        admin_context: true,
        granted: false,
    };
    let input = serde_json::from_value(json!({
        "operation_id": library::random_id(), "expected_revision": 2, "reason": "synthetic hash ban",
    })).unwrap();
    server
        .state
        .storage
        .library_block(actor, &id, &input, true)
        .unwrap();
    assert_eq!(avatar(&server, 111, 3).await.status(), 404);
    let response = avatar(&server, 111, 111).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), copy.as_slice());
    let profile = server
        .client
        .get(format!("{}/api/{}", server.base, Uuid::from_u128(111)))
        .header("token", "synthetic-token-3")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(profile["equipped"].as_array().unwrap().is_empty());

    let scope = server
        .state
        .protocol_sessions
        .issue(
            &server.state,
            &crate::protocol::v1::Auth {
                token: "synthetic-token-3".into(),
                instance_id: server.state.protocol_sessions.instance_id.clone(),
                ..Default::default()
            },
            77,
        )
        .unwrap();
    let response = server
        .client
        .get(format!(
            "{}/api/protobuf/v1/resources/{}/avatar/1",
            server.base,
            Uuid::from_u128(111)
        ))
        .bearer_auth(&scope.session_id)
        .header("x-omnimorph-epoch", "77")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    for (owner, bytes, hidden) in [
        (222, copy.clone(), true),
        (333, repack(&bytes, true), false),
    ] {
        upload(&server, owner, &bytes).await;
        assert_eq!(
            avatar(&server, owner, 3).await.status(),
            if hidden { 404 } else { 200 }
        );
        let response = avatar(&server, owner, owner).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_slice());
    }
    for owner in [111, 222, 333] {
        tokio::fs::remove_file(crate::service::resource::legacy_path(
            Uuid::from_u128(owner),
            false,
        ))
        .await
        .unwrap();
    }
}
