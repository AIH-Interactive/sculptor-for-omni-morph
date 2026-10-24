use super::*;
use crate::{
    protocol::v1::AvatarDescriptor,
    service::{library_policy::Actor, outfit::*, outfit_limits::OutfitLimits},
    storage::{outfit_apply::ApplyInput, outfit_resources::LocalBinding},
};

fn apply(server: &Server, configuration: OutfitConfig) -> OutfitConfig {
    server
        .state
        .storage
        .outfit_apply(
            Actor {
                uuid: Uuid::from_u128(1),
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

fn binding(native: &AvatarDescriptor, accessory: bool) -> LocalBinding {
    LocalBinding {
        reference: ResourceRef::Local {
            local_id: library::random_id().parse().unwrap(),
            content_hash: "d".repeat(64),
            format: "FIGURA".into(),
        },
        asset_id: native.asset_id.clone(),
        asset_revision: native.revision,
        raw_sha256: faster_hex::hex_string(&native.raw_sha256),
        resource_kind: if accessory {
            ResourceKind::Accessory
        } else {
            ResourceKind::Avatar
        },
        accessory_metadata: accessory.then(|| {
            serde_json::from_value(json!({
                "schema_version": 1, "category": "General", "cosmetic_anchor": "head_anchor",
                "default_transform": {"position": [0,0,0], "rotation": [0,0,0], "scale": [1,1,1]},
                "supported_bases": ["VANILLA"], "has_bones": false, "skeleton_revision": "",
                "skeleton_hash": "", "display_bones": false, "edit_bones": false,
                "exported_bones": [], "validation_source": "CLIENT_EXTRACTED"
            }))
            .unwrap()
        }),
    }
}

pub(super) async fn download(
    server: &Server,
    native: &AvatarDescriptor,
    observer: u128,
) -> reqwest::Response {
    let scope = server
        .state
        .protocol_sessions
        .issue(
            &server.state,
            &crate::protocol::v1::Auth {
                token: format!("synthetic-token-{observer}"),
                instance_id: server.state.protocol_sessions.instance_id.clone(),
                ..Default::default()
            },
            77,
        )
        .unwrap();
    server
        .client
        .get(format!(
            "{}/api/protobuf/v1/resources/{}/{}/{}",
            server.base, native.owner, native.asset_id, native.revision
        ))
        .bearer_auth(scope.session_id)
        .header("x-omnimorph-epoch", "77")
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn outfit_private_native_urls_require_current_display_and_keep_unmarked_legacy_behavior() {
    let server = Server::start().await;
    let native = outfit_resources::upload_private(&server, "private-read").await;
    let legacy = outfit_resources::upload_private(&server, "ordinary-legacy-read").await;
    assert_eq!(
        download(&server, &native, 2)
            .await
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        figura_package()
    );
    let binding = binding(&native, false);
    let observer = Actor {
        uuid: Uuid::from_u128(2),
        admin_context: false,
        granted: false,
    };
    server
        .state
        .storage
        .library_resource_access(
            Uuid::from_u128(1),
            &native.asset_id,
            native.revision,
            observer,
        )
        .unwrap();
    server.state.storage.library_transaction(|database| {
        database.execute_batch("CREATE TEMP TRIGGER fail_private_marker BEFORE INSERT ON outfit_private_sources
            BEGIN SELECT RAISE(ABORT, 'simulated private marker write failure'); END;").unwrap();
        Ok(())
    }).unwrap();
    assert_eq!(
        server
            .state
            .storage
            .outfit_bind_local(Uuid::from_u128(1), &binding, &OutfitLimits::default())
            .unwrap_err(),
        ErrorCode::Internal
    );
    server
        .state
        .storage
        .library_transaction(|database| {
            let count: i64 = database
                .query_row("SELECT count(*) FROM outfit_local_resources", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                count, 0,
                "binding and private marker must roll back together"
            );
            database
                .execute_batch("DROP TRIGGER fail_private_marker;")
                .unwrap();
            Ok(())
        })
        .unwrap();
    server
        .state
        .storage
        .outfit_bind_local(Uuid::from_u128(1), &binding, &OutfitLimits::default())
        .unwrap();
    assert_eq!(
        server
            .state
            .storage
            .library_resource_access(
                Uuid::from_u128(1),
                &native.asset_id,
                native.revision,
                observer
            )
            .unwrap_err(),
        ErrorCode::NotFound
    );
    assert_eq!(download(&server, &native, 2).await.status(), 404);
    assert_eq!(
        download(&server, &native, 1)
            .await
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        figura_package()
    );
    let mut configuration = OutfitConfig::default();
    configuration.base = Some(binding.reference);
    configuration.show_equipment = true;
    let mut configuration = apply(&server, configuration);
    assert_eq!(
        download(&server, &native, 2)
            .await
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        figura_package()
    );
    configuration.show_equipment = false;
    let mut configuration = apply(&server, configuration);
    assert_eq!(download(&server, &native, 2).await.status(), 404);
    assert_eq!(
        download(&server, &legacy, 2)
            .await
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        figura_package()
    );
    configuration.show_equipment = true;
    configuration.base = None;
    apply(&server, configuration);
    assert_eq!(download(&server, &native, 2).await.status(), 404);
    assert_eq!(download(&server, &legacy, 2).await.status(), 200);
}

pub(super) fn large_figura() -> Vec<u8> {
    use std::io::{Read, Write};
    let mut nbt = Vec::new();
    flate2::read::GzDecoder::new(figura_package().as_slice())
        .read_to_end(&mut nbt)
        .unwrap();
    assert_eq!(nbt.pop(), Some(0));
    nbt.push(7); // A valid opaque NBT byte-array forces multiple real body chunks.
    nbt.extend_from_slice(&7u16.to_be_bytes());
    nbt.extend_from_slice(b"payload");
    nbt.extend_from_slice(&70000i32.to_be_bytes());
    let mut random = 0x12345678u32;
    for _ in 0..70000 {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        nbt.push(random as u8);
    }
    nbt.push(0);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&nbt).unwrap();
    encoder.finish().unwrap()
}

#[tokio::test]
async fn outfit_projection_rechecks_hash_bans_after_inspecting_an_unverified_body() {
    let server = Server::start().await;
    let native = outfit_resources::upload_private(&server, "old-unverified-source").await;
    let binding = binding(&native, false);
    server
        .state
        .storage
        .outfit_bind_local(Uuid::from_u128(1), &binding, &OutfitLimits::default())
        .unwrap();
    let mut configuration = OutfitConfig::default();
    configuration.base = Some(binding.reference);
    configuration.show_equipment = true;
    apply(&server, configuration);
    let raw = faster_hex::hex_string(&native.raw_sha256);
    server.state.storage.library_transaction(|database| {
        let nbt: String = database.query_row(
            "SELECT verified_hash FROM library_verified_blobs WHERE raw_sha256 = ?1 AND hash_kind = 'NBT_SHA256'",
            [&raw], |row| row.get(0),
        ).unwrap();
        database.execute("INSERT INTO library_wardrobe_blocks
            (operation_id, hash_kind, verified_hash, owner_uuid, asset_id, asset_revision, actor_uuid, created_at, active)
            VALUES (?1, 'NBT_SHA256', ?2, ?3, ?4, ?5, ?6, 1, 1)",
            params![library::random_id(), nbt, native.owner, native.asset_id, native.revision as i64, Uuid::from_u128(4).to_string()]).unwrap();
        // Represent an old imported native body whose package identity has not been indexed.
        database.execute("DELETE FROM library_verified_blobs WHERE raw_sha256 = ?1", [&raw]).unwrap();
        Ok(())
    }).unwrap();
    let snapshot = server
        .state
        .storage
        .outfit_project(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            &server.base,
            &OutfitLimits::default(),
        )
        .unwrap();
    assert!(snapshot.configuration.is_none());
    assert!(snapshot.resources.is_empty());
    server.state.storage.library_read(|database| {
        let restored: bool = database.query_row(
            "SELECT EXISTS(SELECT 1 FROM library_verified_blobs WHERE raw_sha256 = ?1 AND hash_kind = 'NBT_SHA256')",
            [&raw], |row| row.get(0),
        ).unwrap();
        assert!(restored, "final authorization must run after package identity inspection");
        Ok(())
    }).unwrap();
    assert_eq!(download(&server, &native, 2).await.status(), 404);
}

#[tokio::test]
async fn outfit_private_body_stops_on_disable_remove_hide_or_budget_change() {
    use futures_util::StreamExt;
    for change in ["disable", "remove", "hide", "budget"] {
        let server = Server::start().await;
        let bytes = large_figura();
        assert!(bytes.len() > 65536);
        let native = outfit_resources::upload_private_bytes(&server, "private-stream", bytes).await;
        let binding = binding(&native, true);
        server
            .state
            .storage
            .outfit_bind_local(Uuid::from_u128(1), &binding, &OutfitLimits::default())
            .unwrap();
        let mut configuration = OutfitConfig::default();
        configuration.show_equipment = true;
        configuration
            .display_ordinals
            .insert(binding.reference.identity(), 1);
        configuration.instances.push(AccessoryInstance {
            instance_id: Uuid::from_u128(100),
            resource: binding.reference,
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
        let mut configuration = apply(&server, configuration);
        let response = download(&server, &native, 2).await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.bytes().await.unwrap().len() as u64,
            native.raw_length
        );
        let file =
            tokio::fs::File::open(server.state.storage.blob_path(&native.raw_sha256).unwrap())
                .await
                .unwrap();
        let viewer = Uuid::from_u128(2);
        // Exercise the production stream one poll at a time to avoid HTTP prefetch races.
        let body = crate::service::resource::guarded_body(
            server.state.clone(),
            file,
            native.raw_length,
            Some(native.clone()),
            viewer,
            move |_| Ok(viewer),
        )
        .unwrap();
        let mut stream = body.into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().len(), 32768);
        match change {
            "disable" => configuration.instances[0].enabled = false,
            "remove" => configuration.instances.clear(),
            "hide" => configuration.show_equipment = false,
            "budget" => {
                server
                    .state
                    .config
                    .write()
                    .await
                    .outfit
                    .limits
                    .max_instances = 0
            }
            _ => unreachable!(),
        }
        if change != "budget" {
            apply(&server, configuration);
        }
        assert!(stream.next().await.unwrap().is_err(), "{change}");
        drop(stream);
        assert_eq!(
            download(&server, &native, 2).await.status(),
            404,
            "{change}"
        );
        let own = download(&server, &native, 1).await;
        assert_eq!(own.status(), 200, "owner still has private read access");
        assert_eq!(own.bytes().await.unwrap().len() as u64, native.raw_length);
    }
}
