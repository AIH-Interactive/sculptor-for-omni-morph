use super::*;
use crate::{
    service::{
        library_policy::Actor,
        outfit::{OutfitConfig, ResourceRef},
        outfit_limits::OutfitLimits,
    },
    storage::outfit_apply::ApplyInput,
};

fn apply(server: &Server, owner: u128, configuration: OutfitConfig) -> OutfitConfig {
    server
        .state
        .storage
        .outfit_apply(
            Actor {
                uuid: Uuid::from_u128(owner),
                admin_context: false,
                granted: false,
            },
            &server.base,
            &ApplyInput {
                operation_id: library::random_id().parse().unwrap(),
                expected_revision: configuration.revision,
                configuration,
            },
            &OutfitLimits::default(),
        )
        .unwrap()
        .configuration
}

#[tokio::test]
async fn actual_java_observer_reads_rust_figura_snapshot_and_asset() {
    if std::env::var_os("OMNIMORPH_CLIENT_CLASSPATH_FILE").is_none() {
        println!("SKIPPED Java observer: set OMNIMORPH_CLIENT_CLASSPATH_FILE");
        return;
    }
    let server = Server::start().await;
    let (id, bytes) = server.upload_model("PUBLIC").await;
    let hash = faster_hex::hex_string(&crate::storage::sha256(&bytes));
    let mut configuration = OutfitConfig::default();
    configuration.base = Some(ResourceRef::Cloud {
        origin: server.base.clone(),
        resource_id: id.parse().unwrap(),
        revision: 1,
        content_hash: hash.clone(),
        format: "FIGURA".into(),
    });
    configuration.show_equipment = true;
    apply(&server, 1, configuration);
    crate::api::protobuf::tests::java_client::execute(
        &[
            "top.aihmc.omnimorph.backend2.library.OutfitObserverClientSmokeTest".into(),
            "--rust".into(),
            server.base.clone(),
            Uuid::from_u128(1).to_string(),
            hash,
        ],
        server._directory.path(),
    )
    .await;
}

async fn snapshot(server: &Server, owner: u128, viewer: u128) -> Value {
    let response = server
        .request(
            Method::GET,
            &format!("/api/library/v1/outfits/players/{}", Uuid::from_u128(owner)),
            viewer,
        )
        .header("x-omnimorph-origin", &server.base)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert_eq!(response.headers()["cache-control"], "no-store");
    response.json().await.unwrap()
}

async fn body(server: &Server, path: &str, viewer: u128) -> reqwest::Response {
    server
        .request(Method::GET, path, viewer)
        .header("x-omnimorph-origin", &server.base)
        .send()
        .await
        .unwrap()
}

fn grant(server: &Server, model: &str, player: u128, enabled: bool) {
    server
        .state
        .storage
        .library_transaction(|database| {
            database
                .execute(
                    "DELETE FROM library_grants WHERE model_id = ?1 AND player_uuid = ?2",
                    params![model, Uuid::from_u128(player).to_string()],
                )
                .unwrap();
            if enabled {
                database
                    .execute(
                        "INSERT INTO library_grants VALUES (?1, ?2, 'Viewer', ?3, 1)",
                        params![
                            model,
                            Uuid::from_u128(player).to_string(),
                            Uuid::from_u128(1).to_string()
                        ],
                    )
                    .unwrap();
            }
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn outfit_observer_http_private_rendering_is_revision_scoped_and_does_not_grant_catalog_access()
 {
    let server = Server::start().await;
    let (id, bytes) = server.upload_model("PRIVATE").await;
    let mut configuration = OutfitConfig::default();
    configuration.base = Some(ResourceRef::Cloud {
        origin: server.base.clone(),
        resource_id: id.parse().unwrap(),
        revision: 1,
        content_hash: faster_hex::hex_string(&crate::storage::sha256(&bytes)),
        format: "FIGURA".into(),
    });
    configuration.show_equipment = true;
    let mut configuration = apply(&server, 1, configuration);
    let snapshot_path = format!("/api/library/v1/outfits/players/{}", Uuid::from_u128(1));
    assert_eq!(
        server
            .client
            .get(format!("{}{snapshot_path}", server.base))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        server
            .request(Method::GET, &snapshot_path, 2)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let first = snapshot(&server, 1, 2).await;
    assert_eq!(first["schema_version"], 1);
    assert_eq!(
        first["configuration"],
        serde_json::to_value(&configuration).unwrap()
    );
    let resource = &first["resources"][0];
    assert!(resource.get("descriptor").is_none());
    assert!(resource.get("asset_id").is_none());
    assert_eq!(resource["byte_length"], bytes.len());
    assert_eq!(resource["distribution_format"], 1);
    let path = resource["relative_path"].as_str().unwrap();
    let response = body(&server, path, 2).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-omnimorph-sha256"],
        resource["raw_sha256"].as_str().unwrap()
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes);
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{id}"), 2)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(server.request(Method::POST, &format!("/api/library/v1/models/{id}/transfers"), 2)
        .json(&json!({"operation_id": library::random_id(), "purpose": "download", "model_revision": 1}))
        .send().await.unwrap().status(), 404);
    let mut other = OutfitConfig::default();
    other.show_equipment = true;
    apply(&server, 2, other);
    let wrong_owner = path.replace(
        &Uuid::from_u128(1).to_string(),
        &Uuid::from_u128(2).to_string(),
    );
    assert_eq!(body(&server, &wrong_owner, 2).await.status(), 404);
    configuration = apply(&server, 1, configuration);
    assert_eq!(body(&server, path, 2).await.status(), 409);
    let second = snapshot(&server, 1, 2).await;
    assert_eq!(second["resources"][0]["id"], resource["id"]);
    assert_eq!(second["resources"][0]["raw_sha256"], resource["raw_sha256"]);
    assert_eq!(second["equipment_revision"], first["equipment_revision"]);
    configuration.show_equipment = false;
    let hidden = apply(&server, 1, configuration);
    let hidden_path = format!(
        "/api/library/v1/outfits/players/{}/resources/{}?revision={}",
        Uuid::from_u128(1),
        resource["id"].as_str().unwrap(),
        hidden.revision
    );
    assert_eq!(body(&server, &hidden_path, 2).await.status(), 404);
    let hidden_snapshot = snapshot(&server, 1, 2).await;
    assert!(hidden_snapshot["configuration"].is_null());
    assert_eq!(hidden_snapshot["resources"], json!([]));
    server
        .state
        .storage
        .library_read(|database| {
            let receipts: i64 = database
                .query_row(
                    "SELECT count(*) FROM library_download_receipts",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let downloads: i64 = database
                .query_row(
                    "SELECT COALESCE(sum(downloads), 0) FROM library_stats",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!((receipts, downloads), (0, 0));
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn outfit_observer_http_rechecks_both_players_grants_on_an_existing_body_url() {
    let server = Server::start().await;
    let (id, bytes) = server.upload_model("PUBLIC").await;
    server
        .state
        .storage
        .library_transaction(|database| {
            database
                .execute(
                    "UPDATE library_models SET requires_authorization = 1 WHERE model_id = ?1",
                    [&id],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    grant(&server, &id, 2, true);
    let mut configuration = OutfitConfig::default();
    configuration.show_equipment = true;
    configuration.base = Some(ResourceRef::Cloud {
        origin: server.base.clone(),
        resource_id: id.parse().unwrap(),
        revision: 1,
        content_hash: faster_hex::hex_string(&crate::storage::sha256(&bytes)),
        format: "FIGURA".into(),
    });
    apply(&server, 2, configuration);
    let own = snapshot(&server, 2, 2).await;
    let path = own["resources"][0]["relative_path"].as_str().unwrap();
    assert!(snapshot(&server, 2, 3).await["configuration"].is_null());
    assert_eq!(body(&server, path, 3).await.status(), 404);
    grant(&server, &id, 3, true);
    assert_eq!(
        body(&server, path, 3).await.bytes().await.unwrap().as_ref(),
        bytes
    );
    grant(&server, &id, 2, false);
    assert_eq!(body(&server, path, 3).await.status(), 404);
    assert!(snapshot(&server, 2, 3).await["configuration"].is_null());
    grant(&server, &id, 2, true);
    assert_eq!(
        body(&server, path, 3).await.bytes().await.unwrap().as_ref(),
        bytes
    );
    grant(&server, &id, 3, false);
    assert_eq!(body(&server, path, 3).await.status(), 404);
}

#[tokio::test]
async fn outfit_observer_stream_stops_on_configuration_change_hash_ban_or_session_revocation() {
    use axum::extract::{Path, Query, State};
    use futures_util::StreamExt;
    for change in ["revision", "hash-ban", "session"] {
        let server = Server::start().await;
        let bytes = outfit_read_access::large_figura();
        let input = outfit_uploads::input(&bytes);
        let reserved = outfit_uploads::reserve(&server, &input).await;
        assert_eq!(
            server
                .request(Method::PUT, reserved["relative_path"].as_str().unwrap(), 1)
                .header("content-type", "application/octet-stream")
                .body(bytes.clone())
                .send()
                .await
                .unwrap()
                .status(),
            204
        );
        assert_eq!(
            server
                .request(
                    Method::POST,
                    &format!(
                        "/api/library/v1/outfits/resources/uploads/{}/commit",
                        input.operation_id
                    ),
                    1
                )
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let mut configuration = OutfitConfig::default();
        configuration.show_equipment = true;
        configuration.base = Some(input.reference);
        let configuration = apply(&server, 1, configuration);
        let current = snapshot(&server, 1, 2).await;
        let resource = &current["resources"][0];
        assert_eq!(
            body(&server, resource["relative_path"].as_str().unwrap(), 2)
                .await
                .bytes()
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-omnimorph-auth-mode", "figura".parse().unwrap());
        headers.insert("authorization", "Bearer synthetic-token-2".parse().unwrap());
        headers.insert(
            "host",
            server
                .base
                .strip_prefix("http://")
                .unwrap()
                .parse()
                .unwrap(),
        );
        headers.insert("x-omnimorph-origin", server.base.parse().unwrap());
        // Poll the production handler body directly so transport prefetch cannot hide the boundary.
        let response = crate::api::library::outfit_observers::download(
            State(server.state.clone()),
            Path((
                Uuid::from_u128(1),
                resource["id"].as_str().unwrap().to_owned(),
            )),
            Query(crate::api::library::outfit_observers::ResourceQuery {
                revision: configuration.revision,
            }),
            headers,
        )
        .await
        .unwrap_or_else(|_| panic!("authorized outfit handler rejected the stream"));
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().len(), 32768);
        match change {
            "revision" => {
                apply(&server, 1, configuration);
            }
            "hash-ban" => {
                server.state.storage.library_transaction(|database| {
                    database.execute("INSERT INTO library_wardrobe_blocks
                        (operation_id, hash_kind, verified_hash, owner_uuid, asset_id, asset_revision, actor_uuid, created_at, active)
                        VALUES (?1, 'RAW_SHA256', ?2, ?3, ?4, 1, ?5, 1, 1)",
                        params![library::random_id(), resource["raw_sha256"].as_str().unwrap(), Uuid::from_u128(1).to_string(),
                            format!("outfit_{}", input.operation_id.simple()), Uuid::from_u128(4).to_string()]).unwrap();
                    Ok(())
                }).unwrap();
            }
            "session" => {
                let user = server
                    .state
                    .user_manager
                    .get_by_uuid(&Uuid::from_u128(2))
                    .unwrap()
                    .clone();
                server.state.user_manager.ban(&user).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(stream.next().await.unwrap().is_err(), "{change}");
    }
}

#[tokio::test]
async fn outfit_observer_snapshot_accepts_a_valid_near_budget_configuration_plus_resource_declarations()
 {
    use crate::service::outfit::{AccessoryInstance, AnchorRef, Transform};
    let server = Server::start().await;
    let bytes = figura_package();
    let mut input = outfit_uploads::input(&bytes);
    input.resource_kind = crate::service::outfit::ResourceKind::Accessory;
    input.accessory_metadata = Some(
        serde_json::from_value(json!({
            "schema_version": 1, "category": "General", "cosmetic_anchor": "head_anchor",
            "default_transform": {"position": [0,0,0], "rotation": [0,0,0], "scale": [1,1,1]},
            "supported_bases": ["VANILLA"], "has_bones": false, "skeleton_revision": "",
            "skeleton_hash": "", "display_bones": false, "edit_bones": false,
            "exported_bones": [], "validation_source": "CLIENT_EXTRACTED"
        }))
        .unwrap(),
    );
    let reserved = outfit_uploads::reserve(&server, &input).await;
    assert_eq!(
        server
            .request(Method::PUT, reserved["relative_path"].as_str().unwrap(), 1)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!(
                    "/api/library/v1/outfits/resources/uploads/{}/commit",
                    input.operation_id
                ),
                1
            )
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let mut configuration = OutfitConfig::default();
    configuration.show_equipment = true;
    configuration
        .display_ordinals
        .insert(input.reference.identity(), 1);
    configuration.instances.push(AccessoryInstance {
        instance_id: Uuid::from_u128(100),
        resource: input.reference,
        display_ordinal: 1,
        enabled: true,
        anchor: AnchorRef::Standard {
            name: "head_anchor".into(),
        },
        transform: Transform::default(),
        bone_overrides: Vec::new(),
        controls: Default::default(),
        lua_state: Default::default(),
    });
    let configuration = (1..=512)
        .filter_map(|width| {
            let mut candidate = configuration.clone();
            for index in 0..128 {
                candidate.instances[0]
                    .controls
                    .insert(format!("control{index:03}"), json!("x".repeat(width)));
            }
            candidate.validate().is_ok().then_some(candidate)
        })
        .last()
        .unwrap();
    assert!(serde_json::to_vec(&configuration).unwrap().len() > 65000);
    let configuration = apply(&server, 1, configuration);
    let actual = snapshot(&server, 1, 2).await;
    assert!(serde_json::to_vec(&actual).unwrap().len() > 65536);
    assert_eq!(
        actual["configuration"],
        serde_json::to_value(configuration).unwrap()
    );
    assert_eq!(actual["resources"].as_array().unwrap().len(), 1);
}
