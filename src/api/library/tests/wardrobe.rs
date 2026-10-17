use super::*;
use std::io::{Read, Write};

fn repack(bytes: &[u8], change_name: bool) -> Vec<u8> {
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

async fn upload(server: &Server, player: u128, bytes: &[u8]) {
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
