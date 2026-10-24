use super::*;
use crate::protocol::v1::{AvatarDescriptor, CommitUpload, UploadRequest};
use crate::service::resource;

const BIND_PATH: &str = "/api/library/v1/outfits/resources/local";

#[tokio::test]
async fn private_source_body_rechecks_session_binding_ban_and_declaration_budget_per_chunk() {
    use axum::extract::State;
    use futures_util::StreamExt;
    use std::io::{Read, Write};
    let declaration = json!({
        "schema_version": 1, "category": "Hair", "cosmetic_anchor": "head",
        "default_transform": {"position": [1, 0, 0], "rotation": [0, 0, 0], "scale": [1, 1, 1]},
        "supported_bases": ["VANILLA", "FIGURA", "YSM"], "has_bones": false,
        "skeleton_revision": "", "skeleton_hash": "", "display_bones": false,
        "edit_bones": false, "exported_bones": [], "editable_bones": [], "validation_source": "CLIENT_EXTRACTED"
    });
    let mut nbt = Vec::new();
    flate2::read::GzDecoder::new(outfit_read_access::large_figura().as_slice())
        .read_to_end(&mut nbt)
        .unwrap();
    let mut original = Vec::new();
    flate2::read::GzDecoder::new(figura_package().as_slice())
        .read_to_end(&mut original)
        .unwrap();
    let mut fields = Vec::new();
    for (key, value) in [
        ("resource_kind", "ACCESSORY".to_owned()),
        ("cosmetic_json", declaration.to_string()),
    ] {
        fields.push(8);
        fields.extend_from_slice(&(key.len() as u16).to_be_bytes());
        fields.extend_from_slice(key.as_bytes());
        fields.extend_from_slice(&(value.len() as u16).to_be_bytes());
        fields.extend_from_slice(value.as_bytes());
    }
    nbt.splice(original.len() - 2..original.len() - 2, fields);
    let mut compressed = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    compressed.write_all(&nbt).unwrap();
    let bytes = compressed.finish().unwrap();
    for change in ["session", "delete", "ban", "budget"] {
        let server = Server::start().await;
        let native = upload_private_bytes(&server, "stream-source", bytes.clone()).await;
        let mut input = binding(&native);
        input["resource_kind"] = json!("ACCESSORY");
        input["accessory_metadata"] = declaration.clone();
        assert_eq!(
            server
                .request(Method::PUT, BIND_PATH, 1)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-omnimorph-auth-mode", "figura".parse().unwrap());
        headers.insert("authorization", "Bearer synthetic-token-1".parse().unwrap());
        let response = crate::api::library::outfit_local_sources::download(
            State(server.state.clone()),
            headers,
            Ok(Json(crate::api::library::outfit_local_sources::BodyInput {
                reference: serde_json::from_value(input["reference"].clone()).unwrap(),
                resource_kind: crate::service::outfit::ResourceKind::Accessory,
                raw_sha256: input["raw_sha256"].as_str().unwrap().to_owned(),
            })),
        )
        .await
        .unwrap_or_else(|_| panic!("private body handler rejected valid source"));
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().len(), 32768);
        match change {
            "session" => {
                let user = server
                    .state
                    .user_manager
                    .get_by_uuid(&Uuid::from_u128(1))
                    .unwrap()
                    .clone();
                server.state.user_manager.ban(&user).unwrap();
            }
            "delete" => {
                server
                    .state
                    .storage
                    .delete(
                        Uuid::from_u128(1),
                        &crate::protocol::v1::Delete {
                            operation_id: library::random_id(),
                            asset_id: native.asset_id,
                            expected_revision: 1,
                        },
                    )
                    .unwrap();
            }
            "ban" => {
                server.state.storage.library_transaction(|database| {
                database.execute("INSERT INTO library_wardrobe_blocks
                    (operation_id, hash_kind, verified_hash, owner_uuid, asset_id, asset_revision, actor_uuid, created_at, active)
                    VALUES (?1, 'RAW_SHA256', ?2, ?3, ?4, 1, ?5, 1, 1)",
                    params![library::random_id(), input["raw_sha256"].as_str().unwrap(), Uuid::from_u128(1).to_string(),
                        native.asset_id, Uuid::from_u128(4).to_string()]).unwrap();
                Ok(())
            }).unwrap();
            }
            "budget" => {
                server
                    .state
                    .config
                    .write()
                    .await
                    .outfit
                    .limits
                    .position_limit = 0.0;
            }
            _ => unreachable!(),
        }
        assert!(
            stream.next().await.unwrap().is_err(),
            "next chunk must reject {change}"
        );
    }
}

#[tokio::test]
async fn private_binding_lookup_recovers_unworn_sources_without_treating_revocation_as_missing() {
    let server = Server::start().await;
    let path = format!("{BIND_PATH}/resolve");
    let native = upload_private(&server, "lookup-source").await;
    let input = binding(&native);
    let query = json!({"reference": input["reference"], "resource_kind": "AVATAR"});
    assert_eq!(
        server
            .client
            .post(format!("{}{path}", server.base))
            .json(&query)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let response = server
        .request(Method::POST, &path, 1)
        .json(&query)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.json::<Value>().await.unwrap()["resource"].is_null());
    assert_eq!(
        server
            .request(Method::PUT, BIND_PATH, 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    for _ in 0..2 {
        let response = server
            .request(Method::POST, &path, 1)
            .json(&query)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        assert_eq!(response.json::<Value>().await.unwrap()["resource"], input);
    }
    let reopened = Storage::open(server._directory.path()).unwrap();
    let reference = serde_json::from_value(query["reference"].clone()).unwrap();
    assert_eq!(
        reopened
            .outfit_resolve_local(
                Uuid::from_u128(1),
                &reference,
                crate::service::outfit::ResourceKind::Avatar,
                &crate::service::outfit_limits::OutfitLimits::default()
            )
            .unwrap()
            .unwrap()
            .asset_id,
        native.asset_id
    );
    let foreign = server
        .request(Method::POST, &path, 2)
        .json(&query)
        .send()
        .await
        .unwrap();
    assert!(foreign.json::<Value>().await.unwrap()["resource"].is_null());
    let body_path = format!("{BIND_PATH}/body");
    let mut body_query = query.clone();
    body_query["raw_sha256"] = input["raw_sha256"].clone();
    let response = server
        .request(Method::POST, &body_path, 1)
        .json(&body_query)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-omnimorph-sha256"],
        input["raw_sha256"].as_str().unwrap()
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), figura_package());
    assert_eq!(
        server
            .request(Method::POST, &body_path, 2)
            .json(&body_query)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let mut wrong_body = body_query.clone();
    wrong_body["raw_sha256"] = json!("b".repeat(64));
    assert_eq!(
        server
            .request(Method::POST, &body_path, 1)
            .json(&wrong_body)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let mut invalid = query.clone();
    invalid["owner"] = json!(Uuid::from_u128(1));
    assert_eq!(
        server
            .request(Method::POST, &path, 2)
            .json(&invalid)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    invalid = query.clone();
    invalid["resource_kind"] = json!("ACCESSORY");
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&invalid)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    invalid["resource_kind"] = json!("OUTFIT");
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&invalid)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    server.state.storage.library_transaction(|database| {
        database.execute("INSERT INTO library_wardrobe_blocks
            (operation_id, hash_kind, verified_hash, owner_uuid, asset_id, asset_revision, actor_uuid, created_at, active)
            VALUES (?1, 'RAW_SHA256', ?2, ?3, ?4, 1, ?5, 1, 1)",
            params![library::random_id(), input["raw_sha256"].as_str().unwrap(), Uuid::from_u128(1).to_string(),
                native.asset_id, Uuid::from_u128(4).to_string()]).unwrap();
        Ok(())
    }).unwrap();
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&query)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        server
            .request(Method::POST, &body_path, 1)
            .json(&body_query)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    server
        .state
        .storage
        .library_transaction(|database| {
            database
                .execute("UPDATE library_wardrobe_blocks SET active = 0", [])
                .unwrap();
            Ok(())
        })
        .unwrap();
    server
        .state
        .storage
        .delete(
            Uuid::from_u128(1),
            &crate::protocol::v1::Delete {
                operation_id: library::random_id(),
                asset_id: native.asset_id,
                expected_revision: 1,
            },
        )
        .unwrap();
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&query)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .request(Method::POST, &body_path, 1)
            .json(&body_query)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .state
            .storage
            .outfit_current(Uuid::from_u128(1))
            .unwrap()
            .revision,
        0
    );
}

pub(super) async fn upload_private(server: &Server, asset: &str) -> AvatarDescriptor {
    upload_private_bytes(server, asset, figura_package()).await
}

pub(super) async fn upload_private_bytes(
    server: &Server,
    asset: &str,
    bytes: Vec<u8>,
) -> AvatarDescriptor {
    let owner = Uuid::from_u128(1);
    let operation = library::random_id();
    let request = UploadRequest {
        operation_id: operation.clone(),
        resource: Some(AvatarDescriptor {
            owner: owner.to_string(),
            asset_id: asset.into(),
            model_type: 1,
            distribution_format: 1,
            raw_sha256: crate::storage::sha256(&bytes),
            raw_length: bytes.len() as u64,
            ..Default::default()
        }),
    };
    resource::grant_upload(&server.state, owner, &request)
        .await
        .unwrap();
    let mut upload =
        resource::begin_upload(&server.state, owner, operation.parse().unwrap()).unwrap();
    resource::receive_upload(
        &mut upload,
        axum::body::Body::from(bytes.clone()),
        || Ok(()),
    )
    .await
    .unwrap();
    drop(upload);
    resource::commit_upload(
        &server.state,
        owner,
        &CommitUpload {
            operation_id: operation,
            raw_sha256: crate::storage::sha256(&bytes),
            raw_length: bytes.len() as u64,
        },
    )
    .await
    .unwrap()
}

fn binding(avatar: &AvatarDescriptor) -> Value {
    json!({
        "reference": {"source": "LOCAL", "local_id": library::random_id(),
            "content_hash": "a".repeat(64), "format": "FIGURA"},
        "asset_id": avatar.asset_id, "asset_revision": avatar.revision,
        "raw_sha256": faster_hex::hex_string(&avatar.raw_sha256),
        "resource_kind": "AVATAR", "accessory_metadata": null
    })
}

#[tokio::test]
async fn private_binding_is_owner_scoped_replaceable_and_does_not_publish_or_equip() {
    let server = Server::start().await;
    let first = upload_private(&server, "local-first").await;
    let second = upload_private(&server, "local-second").await;
    let input = binding(&first);
    assert_ne!(input["reference"]["content_hash"], input["raw_sha256"]);
    let request =
        |input: &Value, player| server.request(Method::PUT, BIND_PATH, player).json(input);
    let unauthenticated = server
        .client
        .put(format!("{}{BIND_PATH}", server.base))
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), 401);
    assert_eq!(request(&input, 2).send().await.unwrap().status(), 404);
    for _ in 0..2 {
        let response = request(&input, 1).send().await.unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        assert_eq!(response.json::<Value>().await.unwrap(), input);
    }
    // The owner may replace the source's package with another of their own verified private assets.
    let mut changed = input.clone();
    changed["asset_id"] = json!(second.asset_id);
    changed["raw_sha256"] = json!(faster_hex::hex_string(&second.raw_sha256));
    let replaced = request(&changed, 1).send().await.unwrap();
    assert_eq!(replaced.status(), 200, "{}", replaced.text().await.unwrap());
    assert_eq!(replaced.json::<Value>().await.unwrap()["asset_id"], json!(second.asset_id));
    changed = input.clone();
    changed["reference"]["format"] = json!("YSM_PLAIN");
    assert_eq!(request(&changed, 1).send().await.unwrap().status(), 409);
    changed = input.clone();
    changed["raw_sha256"] = json!("0".repeat(64));
    assert_eq!(request(&changed, 1).send().await.unwrap().status(), 409);
    server
        .state
        .storage
        .library_read(|database| {
            let bindings: i64 = database
                .query_row("SELECT count(*) FROM outfit_local_resources", [], |row| {
                    row.get(0)
                })
                .unwrap();
            let catalog: i64 = database
                .query_row("SELECT count(*) FROM library_models", [], |row| row.get(0))
                .unwrap();
            assert_eq!(bindings, 1);
            assert_eq!(catalog, 0);
            Ok(())
        })
        .unwrap();
    assert!(
        server
            .state
            .storage
            .equipped(Uuid::from_u128(1))
            .unwrap()
            .primary
            .is_none()
    );
    assert_eq!(
        server
            .state
            .storage
            .outfit_current(Uuid::from_u128(1))
            .unwrap()
            .revision,
        0
    );
    server
        .state
        .storage
        .delete(
            Uuid::from_u128(1),
            &crate::protocol::v1::Delete {
                operation_id: library::random_id(),
                asset_id: first.asset_id,
                expected_revision: first.revision,
            },
        )
        .unwrap();
    assert_eq!(request(&input, 1).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn private_binding_rejects_catalog_aliases_and_marks_declarations_client_extracted() {
    let server = Server::start().await;
    let (model, _) = server.upload_model("PUBLIC").await;
    let catalog = server
        .state
        .storage
        .avatar(
            Uuid::from_u128(1),
            &format!("library_{}", model.replace('-', "")),
            1,
        )
        .unwrap();
    let rejected = server
        .request(Method::PUT, BIND_PATH, 1)
        .json(&binding(&catalog))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 403);
    let native = upload_private(&server, "local-accessory").await;
    let mut input = binding(&native);
    input["resource_kind"] = json!("ACCESSORY");
    assert_eq!(
        server
            .request(Method::PUT, BIND_PATH, 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    input["accessory_metadata"] = json!({
        "schema_version": 1, "category": "hat", "cosmetic_anchor": "Head",
        "default_transform": {"position": [0, 0, 0], "rotation": [0, 0, 0], "scale": [1, 1, 1]},
        "supported_bases": ["VANILLA", "FIGURA", "YSM"], "has_bones": false,
        "skeleton_revision": "", "skeleton_hash": "", "display_bones": false,
        "edit_bones": false, "exported_bones": [], "validation_source": "SERVER_VERIFIED"
    });
    input["accessory_metadata"]["default_transform"]["position"] = json!([1, 0, 0]);
    let response = server
        .request(Method::PUT, BIND_PATH, 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let stored: Value = response.json().await.unwrap();
    assert_eq!(
        stored["accessory_metadata"]["validation_source"],
        "CLIENT_EXTRACTED"
    );
    let response = server
        .request(Method::PUT, BIND_PATH, 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), stored);
    let query = json!({"reference": input["reference"], "resource_kind": "ACCESSORY"});
    let path = format!("{BIND_PATH}/resolve");
    server
        .state
        .config
        .write()
        .await
        .outfit
        .limits
        .position_limit = 0.0;
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&query)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    server
        .state
        .config
        .write()
        .await
        .outfit
        .limits
        .position_limit = 4096.0;
    let response = server
        .request(Method::POST, &path, 1)
        .json(&query)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["resource"], stored);
}

#[tokio::test]
async fn local_apply_requires_binding_preserves_native_identity_and_rechecks_blocks() {
    use crate::service::outfit::OutfitConfig;
    use crate::storage::outfit_apply::ApplyInput;
    let server = Server::start().await;
    let native = upload_private(&server, "local-base").await;
    let binding = binding(&native);
    let actor = crate::service::library_policy::Actor {
        uuid: Uuid::from_u128(1),
        admin_context: false,
        granted: false,
    };
    let limits = crate::service::outfit_limits::OutfitLimits::default();
    let mut configuration = OutfitConfig::default();
    configuration.base = Some(serde_json::from_value(binding["reference"].clone()).unwrap());
    configuration.show_equipment = false;
    let input = ApplyInput {
        operation_id: library::random_id().parse().unwrap(),
        expected_revision: 0,
        configuration,
    };
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &input, &limits)
            .unwrap_err(),
        ErrorCode::NotFound
    );
    let response = server
        .request(Method::PUT, BIND_PATH, 1)
        .json(&binding)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let applied = server
        .state
        .storage
        .outfit_apply(actor, &server.base, &input, &limits)
        .unwrap();
    assert_eq!(applied.configuration.revision, 1);
    let equipped = server.state.storage.equipped(actor.uuid).unwrap();
    assert_eq!(equipped.primary.as_ref(), Some(&native));
    assert_eq!(equipped.figura_compatibility.as_ref(), Some(&native));
    server.state.storage.library_read(|database| {
        let visible: bool = database.query_row(
            "SELECT show_equipment FROM library_wardrobe_settings WHERE owner_uuid = ?1 AND asset_id = ?2 AND asset_revision = ?3",
            params![actor.uuid.to_string(), native.asset_id, i64::try_from(native.revision).unwrap()], |row| row.get(0)).unwrap();
        assert!(!visible);
        Ok(())
    }).unwrap();
    let clear = ApplyInput {
        operation_id: library::random_id().parse().unwrap(),
        expected_revision: 1,
        configuration: OutfitConfig {
            revision: 1,
            ..Default::default()
        },
    };
    server
        .state
        .storage
        .outfit_apply(actor, &server.base, &clear, &limits)
        .unwrap();
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &input, &limits)
            .unwrap(),
        applied
    );
    assert!(
        server
            .state
            .storage
            .equipped(actor.uuid)
            .unwrap()
            .primary
            .is_none()
    );
    server.state.storage.library_transaction(|transaction| {
        transaction.execute("INSERT INTO library_wardrobe_blocks
            (operation_id, hash_kind, verified_hash, owner_uuid, asset_id, asset_revision, actor_uuid, created_at, active)
            VALUES (?1, 'RAW_SHA256', ?2, ?3, ?4, ?5, ?6, 1, 1)",
            params![library::random_id(), binding["raw_sha256"].as_str().unwrap(), actor.uuid.to_string(), native.asset_id, i64::try_from(native.revision).unwrap(), Uuid::from_u128(4).to_string()]).unwrap();
        Ok(())
    }).unwrap();
    let mut blocked = input.clone();
    blocked.operation_id = library::random_id().parse().unwrap();
    blocked.expected_revision = 2;
    blocked.configuration.revision = 2;
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &blocked, &limits)
            .unwrap_err(),
        ErrorCode::Forbidden
    );
    assert_eq!(
        server
            .request(Method::PUT, BIND_PATH, 1)
            .json(&binding)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        server
            .state
            .storage
            .outfit_current(actor.uuid)
            .unwrap()
            .revision,
        2
    );
    assert!(
        server
            .state
            .storage
            .equipped(actor.uuid)
            .unwrap()
            .primary
            .is_none()
    );
}
