use super::*;
use crate::{
    service::{library_policy::Actor, outfit::*, outfit_limits::OutfitLimits},
    storage::{outfit_apply::ApplyInput, outfit_projection::Snapshot},
};

#[tokio::test]
async fn missing_local_figura_source_reports_not_distributable_without_exposing_reference() {
    let server = Server::start().await;
    let native = outfit_resources::upload_private(&server, "missing-local-figura").await;
    let reference = ResourceRef::Local {
        local_id: library::random_id().parse().unwrap(),
        content_hash: "d".repeat(64),
        format: "FIGURA".into(),
    };
    server
        .state
        .storage
        .outfit_bind_local(
            Uuid::from_u128(1),
            &crate::storage::outfit_resources::LocalBinding {
                reference: reference.clone(),
                asset_id: native.asset_id,
                asset_revision: native.revision,
                raw_sha256: faster_hex::hex_string(&native.raw_sha256),
                resource_kind: ResourceKind::Avatar,
                accessory_metadata: None,
            },
            &OutfitLimits::default(),
        )
        .unwrap();
    let mut configuration = OutfitConfig::default();
    configuration.base = Some(reference);
    configuration.show_equipment = true;
    apply(&server, 1, configuration);
    // Reproduce a persisted local selection whose uploaded source is no longer available.
    server
        .state
        .storage
        .library_transaction(|database| {
            database
                .execute(
                    "DELETE FROM outfit_local_resources WHERE owner_uuid = ?1",
                    [Uuid::from_u128(1).to_string()],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    let response = server
        .request(
            Method::GET,
            &format!("/api/library/v1/outfits/players/{}", Uuid::from_u128(1)),
            2,
        )
        .header("x-omnimorph-origin", &server.base)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let snapshot = response.json::<Value>().await.unwrap();
    assert_eq!(snapshot["distribution_code"], "NOT_DISTRIBUTABLE");
    assert!(snapshot["configuration"].is_null());
    assert_eq!(snapshot["resources"], json!([]));
}

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

fn project(server: &Server, owner: u128, observer: u128) -> Snapshot {
    server
        .state
        .storage
        .outfit_project(
            Uuid::from_u128(owner),
            Uuid::from_u128(observer),
            &server.base,
            &OutfitLimits::default(),
        )
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
                        "INSERT INTO library_grants VALUES (?1, ?2, 'Observer', ?3, 1)",
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

fn reference(server: &Server, id: &str, bytes: &[u8]) -> ResourceRef {
    ResourceRef::Cloud {
        origin: server.base.clone(),
        resource_id: id.parse().unwrap(),
        revision: 1,
        content_hash: faster_hex::hex_string(&crate::storage::sha256(bytes)),
        format: "FIGURA".into(),
    }
}

#[tokio::test]
async fn outfit_projection_rechecks_wearer_and_observer_grants_independently() {
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
    configuration.base = Some(reference(&server, &id, &bytes));
    configuration.show_equipment = true;
    let configuration = apply(&server, 2, configuration);
    assert!(project(&server, 2, 3).configuration.is_none());
    assert!(project(&server, 2, 2).configuration.is_some());
    grant(&server, &id, 3, true);
    let snapshot = project(&server, 2, 3);
    assert_eq!(snapshot.configuration.unwrap(), configuration);
    assert_eq!(snapshot.resources.len(), 1);
    assert_eq!(
        snapshot.resources[0].descriptor.raw_sha256,
        crate::storage::sha256(&bytes)
    );
    grant(&server, &id, 2, false);
    let revoked = project(&server, 2, 3);
    assert!(revoked.configuration.is_none());
    assert!(revoked.resources.is_empty());
    assert!(project(&server, 2, 2).configuration.is_none());
    grant(&server, &id, 2, true);
    assert!(project(&server, 2, 3).configuration.is_some());
    server
        .state
        .storage
        .library_transaction(|database| {
            database
                .execute(
                    "UPDATE library_models SET moderation_status = 'BLOCKED', visibility = 'PRIVATE' WHERE model_id = ?1",
                    [&id],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    assert!(project(&server, 2, 3).configuration.is_none());
}

#[tokio::test]
async fn outfit_projection_private_opt_in_is_render_only_and_keeps_stable_source_identity() {
    let server = Server::start().await;
    let (id, bytes) = server.upload_model("PRIVATE").await;
    let mut configuration = OutfitConfig::default();
    configuration.base = Some(reference(&server, &id, &bytes));
    let mut configuration = apply(&server, 1, configuration);
    assert!(project(&server, 1, 2).configuration.is_none());
    let own = project(&server, 1, 1);
    let identity = own.resources[0].id.clone();
    let native_revision = own.equipment_revision;
    configuration.show_equipment = true;
    let mut configuration = apply(&server, 1, configuration);
    let visible = project(&server, 1, 2);
    assert_eq!(visible.resources[0].id, identity);
    assert_eq!(visible.equipment_revision, native_revision);
    assert_eq!(visible.configuration.as_ref(), Some(&configuration));
    let json = serde_json::to_value(&visible).unwrap();
    assert!(json["resources"][0].get("descriptor").is_none());
    assert!(json["resources"][0].get("asset_id").is_none());
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{id}"), 2)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    configuration.show_equipment = false;
    let configuration = apply(&server, 1, configuration);
    let hidden = project(&server, 1, 2);
    assert_eq!(hidden.revision, configuration.revision);
    assert!(hidden.configuration.is_none());
    assert!(hidden.resources.is_empty());
    assert_eq!(project(&server, 1, 1).resources[0].id, identity);
}

#[tokio::test]
async fn outfit_projection_local_duplicates_preserve_instance_ids_and_hide_disabled_copies() {
    let server = Server::start().await;
    let native = outfit_resources::upload_private(&server, "projection-local").await;
    let reference = ResourceRef::Local {
        local_id: library::random_id().parse().unwrap(),
        content_hash: "c".repeat(64),
        format: "FIGURA".into(),
    };
    let metadata: AccessoryMetadata = serde_json::from_value(json!({
        "schema_version": 1, "category": "General", "cosmetic_anchor": "head_anchor",
        "default_transform": {"position": [0,0,0], "rotation": [0,0,0], "scale": [1,1,1]},
        "supported_bases": ["VANILLA"], "has_bones": false, "skeleton_revision": "",
        "skeleton_hash": "", "display_bones": false, "edit_bones": false,
        "exported_bones": [], "validation_source": "CLIENT_EXTRACTED"
    }))
    .unwrap();
    server
        .state
        .storage
        .outfit_bind_local(
            Uuid::from_u128(1),
            &crate::storage::outfit_resources::LocalBinding {
                reference: reference.clone(),
                asset_id: native.asset_id.clone(),
                asset_revision: native.revision,
                raw_sha256: faster_hex::hex_string(&native.raw_sha256),
                resource_kind: ResourceKind::Accessory,
                accessory_metadata: Some(metadata),
            },
            &OutfitLimits::default(),
        )
        .unwrap();
    let mut configuration = OutfitConfig::default();
    configuration.show_equipment = true;
    configuration
        .display_ordinals
        .insert(reference.identity(), 3);
    for ordinal in 1..=3 {
        configuration.instances.push(AccessoryInstance {
            instance_id: Uuid::from_u128(100 + ordinal as u128),
            resource: reference.clone(),
            display_ordinal: ordinal,
            enabled: ordinal != 2,
            anchor: AnchorRef::Standard {
                name: "head_anchor".into(),
            },
            transform: Transform::default(),
            bone_overrides: Vec::new(),
            controls: Default::default(),
            lua_state: Default::default(),
        });
    }
    let committed = apply(&server, 1, configuration);
    let projected = project(&server, 1, 2);
    let configuration = projected.configuration.unwrap();
    assert_eq!(
        configuration.instances,
        vec![
            committed.instances[0].clone(),
            committed.instances[2].clone()
        ]
    );
    assert_eq!(configuration.display_ordinals[&reference.identity()], 3);
    assert_eq!(projected.resources.len(), 1);
    assert_eq!(
        projected.resources[0].descriptor.raw_sha256,
        native.raw_sha256
    );
    assert_eq!(
        server
            .state
            .storage
            .outfit_current(Uuid::from_u128(1))
            .unwrap(),
        committed
    );
    assert!(project(&server, 1, 3).configuration.is_some());
    instance_state_roundtrip(&server, &committed).await;
    if std::env::var_os("OMNIMORPH_CLIENT_CLASSPATH_FILE").is_some() {
        crate::api::protobuf::tests::java_client::execute(
            &[
                "top.aihmc.omnimorph.backend2.library.OutfitEventsSmokeTest".into(),
                server.base.clone(),
                server.state.storage.instance_id(),
                committed.instances[0].instance_id.to_string(),
            ],
            server._directory.path(),
        )
        .await;
    }
    let mut limits = OutfitLimits::default();
    limits.max_instances = 1;
    let tightened = server
        .state
        .storage
        .outfit_project(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            &server.base,
            &limits,
        )
        .unwrap();
    assert!(tightened.configuration.is_none());
    assert!(tightened.resources.is_empty());
    // A stale native primary cannot silently change an accessory-only composition's base.
    server
        .state
        .storage
        .library_transaction(|database| {
            database
                .execute(
                    "INSERT INTO equipment VALUES (?1, 'primary', ?2, ?3)",
                    params![
                        Uuid::from_u128(1).to_string(),
                        native.asset_id,
                        native.revision as i64
                    ],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    assert!(project(&server, 1, 2).configuration.is_none());
}

async fn instance_state_roundtrip(server: &Server, configuration: &OutfitConfig) {
    let mut observer = super::outfit_events::Peer::connect(server, 2, false).await;
    assert_eq!(
        observer.subscribe(Uuid::from_u128(1)).await["type"],
        "CHANGED"
    );
    let instance = configuration.instances[0].instance_id;
    let read_path = format!(
        "/api/library/v1/outfits/players/{}/instances/{instance}/state",
        Uuid::from_u128(1)
    );
    let write_path = format!("/api/library/v1/outfits/instances/{instance}/state");
    let baseline = server
        .request(Method::GET, &read_path, 2)
        .header("x-omnimorph-origin", &server.base)
        .send()
        .await
        .unwrap();
    assert_eq!(baseline.status(), 200);
    let mut input: Value = baseline.json().await.unwrap();
    assert_eq!(input["revision"], 0);
    input["state"]["generation"] = json!(library::random_id());
    input["state"]["sequence"] = json!(1);
    input["state"]["controls"] = json!({"enabled": true});
    input["state"]["lua_state"] = json!({"mode": "orbit"});
    for _ in 0..2 {
        let response = server
            .request(Method::POST, &write_path, 1)
            .header("x-omnimorph-origin", &server.base)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        assert_eq!(response.json::<Value>().await.unwrap()["revision"], 1);
        let notification = observer.next().await;
        assert_eq!(notification["type"], "STATE_CHANGED");
        assert_eq!(notification["instance_id"], instance.to_string());
        assert_eq!(notification["outfit_revision"], configuration.revision);
        assert_eq!(notification["state_revision"], 1);
        assert!(notification.get("state").is_none());
    }
    let observed = server
        .request(Method::GET, &read_path, 2)
        .header("x-omnimorph-origin", &server.base)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(observed["state"], input["state"]);
    assert_eq!(
        server
            .state
            .storage
            .outfit_current(Uuid::from_u128(1))
            .unwrap(),
        *configuration
    );
    let other_path = read_path.replace(
        &instance.to_string(),
        &configuration.instances[2].instance_id.to_string(),
    );
    let other = server
        .request(Method::GET, &other_path, 2)
        .header("x-omnimorph-origin", &server.base)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(other["revision"], 0);
    assert_eq!(other["state"]["controls"], json!({}));
    let forbidden = server
        .request(Method::POST, &write_path, 2)
        .header("x-omnimorph-origin", &server.base)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert!(matches!(forbidden.status().as_u16(), 403 | 404));
    input["revision"] = json!(1);
    input["state"]["sequence"] = json!(3);
    let gap = server
        .request(Method::POST, &write_path, 1)
        .header("x-omnimorph-origin", &server.base)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(gap.status(), 409);
    input["state"]["sequence"] = json!(2);
    input["state"]["lua_state"] = json!({"oversize": "x".repeat(513)});
    let budget = server
        .request(Method::POST, &write_path, 1)
        .header("x-omnimorph-origin", &server.base)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(budget.status(), 400);
    let mut hidden = configuration.clone();
    hidden.show_equipment = false;
    let mut hidden = apply(server, 1, hidden);
    let denied = server
        .request(Method::GET, &read_path, 2)
        .header("x-omnimorph-origin", &server.base)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 404);
    hidden.show_equipment = true;
    apply(server, 1, hidden);
    let reset = server
        .request(Method::GET, &read_path, 2)
        .header("x-omnimorph-origin", &server.base)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(reset["revision"], 0);
}
