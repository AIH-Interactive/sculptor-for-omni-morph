use super::*;
use crate::{
    service::{library_policy::Actor, outfit::*, outfit_limits::OutfitLimits},
    storage::outfit_apply::ApplyInput,
};

fn request(configuration: OutfitConfig) -> ApplyInput {
    ApplyInput {
        operation_id: library::random_id().parse().unwrap(),
        expected_revision: configuration.revision,
        configuration,
    }
}

#[tokio::test]
async fn native_java_accessory_declarations_are_rechecked_for_cloud_and_private_apply() {
    let Ok(directory) = std::env::var("OMNIMORPH_TEST_ACCESSORY_UPLOAD_DIRECTORY") else {
        eprintln!(
            "SKIPPED native Java author declarations: set OMNIMORPH_TEST_ACCESSORY_UPLOAD_DIRECTORY"
        );
        return;
    };
    let directory = std::path::Path::new(&directory).join("figura");
    let bytes = std::fs::read(directory.join("body.nbt")).unwrap();
    let mut input: Value =
        serde_json::from_slice(&std::fs::read(directory.join("reserve.json")).unwrap()).unwrap();
    input["operation_id"] = json!(library::random_id());
    input["metadata"]["visibility"] = json!("PUBLIC");
    input["metadata"]["requires_authorization"] = json!(false);
    let declaration = input["metadata"]["accessory_metadata"].clone();
    assert_eq!(declaration["category"], "Trail");
    assert_eq!(declaration["has_bones"], false);
    let server = Server::start().await;
    let reserved = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(reserved.status(), 200, "{}", reserved.text().await.unwrap());
    let reserved: Value = reserved.json().await.unwrap();
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
    let committed = server
        .request(
            Method::POST,
            &format!(
                "/api/library/v1/uploads/{}/commit",
                input["operation_id"].as_str().unwrap()
            ),
            1,
        )
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        committed.status(),
        200,
        "{}",
        committed.text().await.unwrap()
    );
    let model: Uuid = reserved["model_id"].as_str().unwrap().parse().unwrap();
    let cloud = ResourceRef::Cloud {
        origin: server.base.clone(),
        resource_id: model,
        revision: 1,
        content_hash: faster_hex::hex_string(&crate::storage::sha256(&bytes)),
        format: "FIGURA".into(),
    };
    let native = outfit_resources::upload_private_bytes(&server, "author-private", bytes).await;
    let local = ResourceRef::Local {
        local_id: library::random_id().parse().unwrap(),
        content_hash: "b".repeat(64),
        format: "FIGURA".into(),
    };
    let bound = server
        .request(Method::PUT, "/api/library/v1/outfits/resources/local", 1)
        .json(&json!({
            "reference": local, "asset_id": native.asset_id, "asset_revision": native.revision,
            "raw_sha256": faster_hex::hex_string(&native.raw_sha256), "resource_kind": "ACCESSORY",
            "accessory_metadata": declaration,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bound.status(), 200, "{}", bound.text().await.unwrap());
    let mut configuration = OutfitConfig::default();
    for resource in [cloud.clone(), local.clone()] {
        configuration
            .display_ordinals
            .insert(resource.identity(), 2);
        for ordinal in 1..=2 {
            configuration.instances.push(AccessoryInstance {
                instance_id: Uuid::from_u128(100 + configuration.instances.len() as u128),
                resource: resource.clone(),
                display_ordinal: ordinal,
                enabled: true,
                anchor: AnchorRef::Standard {
                    name: "head_anchor".into(),
                },
                transform: Transform::default(),
                bone_overrides: Vec::new(),
                controls: Default::default(),
                lua_state: Default::default(),
            });
        }
    }
    configuration.validate().unwrap();
    let actor = Actor {
        uuid: Uuid::from_u128(1),
        admin_context: false,
        granted: false,
    };
    let other = Actor {
        uuid: Uuid::from_u128(2),
        ..actor
    };
    let limits = OutfitLimits::default();
    let first = request(configuration.clone());
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(other, &server.base, &first, &limits)
            .unwrap_err(),
        ErrorCode::NotFound
    );
    server
        .state
        .storage
        .library_read(|database| {
            database
                .execute_batch(
                    "CREATE TEMP TRIGGER change_author_before_commit BEFORE INSERT ON avatars
            BEGIN UPDATE library_accessory_metadata SET metadata_json = json_set(metadata_json,
            '$.supported_bases', json('[\"FIGURA\"]')); END;",
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &first, &limits)
            .unwrap_err(),
        ErrorCode::UnsupportedCapability
    );
    assert_eq!(
        server
            .state
            .storage
            .outfit_current(actor.uuid)
            .unwrap()
            .revision,
        0
    );
    let detail = server
        .request(Method::GET, &format!("/api/library/v1/models/{model}"), 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(
        detail["accessory_metadata"]["supported_bases"]
            .as_array()
            .unwrap()
            .contains(&json!("VANILLA"))
    );
    server
        .state
        .storage
        .library_read(|database| {
            database
                .execute_batch("DROP TRIGGER change_author_before_commit;")
                .unwrap();
            Ok(())
        })
        .unwrap();
    let mut tight = limits.clone();
    tight.max_trails = 1;
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &first, &tight)
            .unwrap_err(),
        ErrorCode::InvalidMessage
    );
    let applied = server
        .state
        .storage
        .outfit_apply(actor, &server.base, &first, &limits)
        .unwrap();
    assert_eq!(applied.configuration.revision, 1);
    assert_eq!(applied.configuration.instances.len(), 4);
    assert_eq!(applied.equipment_revision, 0);
    let parent = BoneRef {
        instance_id: Some(configuration.instances[0].instance_id),
        path: vec![BoneSegment {
            name: "Root".into(),
            sibling_ordinal: 0,
        }],
        skeleton_hash: "a".repeat(64),
    };
    for local_index in [0, 2] {
        let mut overridden = applied.configuration.clone();
        let instance_id = overridden.instances[local_index].instance_id;
        overridden.instances[local_index]
            .bone_overrides
            .push(BoneOverride {
                bone: BoneRef {
                    instance_id: Some(instance_id),
                    ..parent.clone()
                },
                transform: Transform::default(),
            });
        assert_eq!(
            server
                .state
                .storage
                .outfit_apply(actor, &server.base, &request(overridden), &limits)
                .unwrap_err(),
            ErrorCode::Forbidden
        );
    }
    let mut mounted = applied.configuration.clone();
    mounted.instances[1].anchor = AnchorRef::AccessoryBone {
        bone: parent.clone(),
    };
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &request(mounted), &limits)
            .unwrap_err(),
        ErrorCode::Forbidden
    );
    let mut virtual_parent = applied.configuration.clone();
    virtual_parent.virtual_anchors.push(VirtualAnchor {
        id: Uuid::from_u128(200),
        name: "Private parent".into(),
        parent,
        transform: Transform::default(),
    });
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &request(virtual_parent), &limits)
            .unwrap_err(),
        ErrorCode::Forbidden
    );
    assert_eq!(
        server.state.storage.outfit_current(actor.uuid).unwrap(),
        applied.configuration
    );
    // Projection checks each resource independently using a real Java Figura body.
    let mut visible = applied.configuration.clone();
    visible.show_equipment = true;
    let visible = server
        .state
        .storage
        .outfit_apply(actor, &server.base, &request(visible), &limits)
        .unwrap();
    let project = || {
        server
            .state
            .storage
            .outfit_project(actor.uuid, other.uuid, &server.base, &limits)
            .unwrap()
    };
    let before = project();
    assert_eq!(before.configuration.as_ref().unwrap().instances.len(), 4);
    assert_eq!(before.resources.len(), 2);
    server
        .state
        .storage
        .library_transaction(|database| {
            database
                .execute(
                    "UPDATE library_models SET requires_authorization = 1 WHERE model_id = ?1",
                    [model.to_string()],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    let filtered = project();
    assert_eq!(filtered.resources.len(), 1);
    let filtered_configuration = filtered.configuration.unwrap();
    assert_eq!(
        filtered_configuration.instances,
        visible.configuration.instances[2..].to_vec()
    );
    assert!(
        !filtered_configuration
            .display_ordinals
            .contains_key(&cloud.identity())
    );
    assert_eq!(
        filtered_configuration.display_ordinals[&local.identity()],
        2
    );
    server
        .state
        .storage
        .library_transaction(|database| {
            database
                .execute(
                    "INSERT INTO library_grants VALUES (?1, ?2, 'Observer', ?3, 1)",
                    params![
                        model.to_string(),
                        other.uuid.to_string(),
                        actor.uuid.to_string()
                    ],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    let restored = project();
    assert_eq!(restored.configuration.unwrap(), visible.configuration);
    assert_eq!(
        restored
            .resources
            .iter()
            .map(|resource| &resource.id)
            .collect::<Vec<_>>(),
        before
            .resources
            .iter()
            .map(|resource| &resource.id)
            .collect::<Vec<_>>()
    );
}
