use super::*;
use crate::service::outfit::OutfitConfig;

pub(super) fn seed_configuration(server: &Server, owner: Uuid, revision: u64) {
    let mut configuration: OutfitConfig = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/outfit-v1/duplicate-instances.json"
    ))
    .unwrap();
    configuration.revision = revision;
    server
        .state
        .storage
        .library_transaction(|transaction| {
            transaction
                .execute(
                    "INSERT INTO outfit_configurations VALUES (?1, ?2, ?3)
            ON CONFLICT(owner_uuid) DO UPDATE SET revision = excluded.revision,
            configuration_json = excluded.configuration_json",
                    params![
                        owner.to_string(),
                        i64::try_from(revision).unwrap(),
                        serde_json::to_string(&configuration).unwrap()
                    ],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn legacy_http_clear_handles_accessory_only_native_and_catalog_equipment() {
    let server = Server::start().await;
    server.state.config.write().await.token = Some("legacy-test-admin".into());
    let owner = Uuid::from_u128(1);
    let path = format!("/api/v1/avatar/{owner}");
    let clear = || {
        server
            .client
            .delete(format!("{}{path}", server.base))
            .header("token", "legacy-test-admin")
    };
    seed_configuration(&server, owner, 4);
    let denied = server
        .client
        .delete(format!("{}{path}", server.base))
        .header("token", "wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap().revision,
        4
    );
    assert_eq!(clear().send().await.unwrap().status(), 200);
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap(),
        OutfitConfig {
            revision: 5,
            ..Default::default()
        }
    );

    let native = outfit_resources::upload_private(&server, "native-http-clear").await;
    let equipped = server
        .state
        .storage
        .equip(
            owner,
            &crate::protocol::v1::Equip {
                operation_id: library::random_id(),
                asset_id: native.asset_id.clone(),
                asset_revision: native.revision,
                expected_revision: 0,
                ..Default::default()
            },
        )
        .unwrap();
    seed_configuration(&server, owner, 7);
    assert_eq!(clear().send().await.unwrap().status(), 200);
    assert!(
        server
            .state
            .storage
            .equipped(owner)
            .unwrap()
            .primary
            .is_none()
    );
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap().revision,
        8
    );
    assert_eq!(
        server
            .state
            .storage
            .avatar(owner, &native.asset_id, native.revision)
            .unwrap_err(),
        ErrorCode::NotFound
    );

    let (id, _) = server.upload_model("PUBLIC").await;
    let response = server
        .request(
            Method::POST,
            &format!("/api/library/v1/models/{id}/equip"),
            1,
        )
        .json(
            &json!({"operation_id": library::random_id(), "model_revision": 1,
            "expected_equipment_revision": equipped.revision + 1}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let alias = server
        .state
        .storage
        .equipped(owner)
        .unwrap()
        .primary
        .unwrap();
    seed_configuration(&server, owner, 9);
    assert_eq!(clear().send().await.unwrap().status(), 200);
    assert!(
        server
            .state
            .storage
            .equipped(owner)
            .unwrap()
            .primary
            .is_none()
    );
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap(),
        OutfitConfig {
            revision: 10,
            ..Default::default()
        }
    );
    assert_eq!(
        server
            .state
            .storage
            .avatar(owner, &alias.asset_id, alias.revision)
            .unwrap(),
        alias
    );
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{id}"), 1)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn legacy_upload_replaces_the_outfit_while_unchanged_import_reads_preserve_it() {
    let server = Server::start().await;
    server.state.config.write().await.token = Some("legacy-test-admin".into());
    let owner = Uuid::from_u128(31_031);
    let path = format!("/api/v1/avatar/{owner}");
    let bytes = figura_package();
    let upload = || {
        server
            .client
            .put(format!("{}{path}", server.base))
            .header("token", "legacy-test-admin")
            .body(bytes.clone())
    };
    seed_configuration(&server, owner, 4);
    assert_eq!(upload().send().await.unwrap().status(), 200);
    let first = server.state.storage.outfit_current(owner).unwrap();
    assert_eq!(first.revision, 5);
    assert!(first.base.is_some());
    assert!(first.instances.is_empty());
    let equipped = server.state.storage.equipped(owner).unwrap();
    let native = equipped.primary.as_ref().unwrap();
    assert_eq!(native.asset_id, "avatar");
    assert_eq!(native.revision, 1);
    assert_eq!(native.raw_sha256, crate::storage::sha256(&bytes));
    seed_configuration(&server, owner, 6);
    for _ in 0..2 {
        crate::service::resource::ensure_legacy_import(&server.state, owner, false)
            .await
            .unwrap();
        let configuration = server.state.storage.outfit_current(owner).unwrap();
        assert_eq!(configuration.revision, 6);
        assert!(!configuration.instances.is_empty());
    }
    assert_eq!(upload().send().await.unwrap().status(), 200);
    let replaced = server.state.storage.outfit_current(owner).unwrap();
    assert_eq!(replaced.revision, 7);
    assert_eq!(replaced.base, first.base);
    assert!(replaced.instances.is_empty());
    assert_eq!(server.state.storage.equipped(owner).unwrap(), equipped);
    let cleared = server
        .client
        .delete(format!("{}{path}", server.base))
        .header("token", "legacy-test-admin")
        .send()
        .await
        .unwrap();
    assert_eq!(cleared.status(), 200);
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap().revision,
        8
    );
    assert!(!crate::service::resource::legacy_read_path(owner, false).exists());
}

#[tokio::test]
async fn failed_legacy_upload_preserves_the_file_and_cannot_activate_on_a_later_read() {
    let server = Server::start().await;
    server.state.config.write().await.token = Some("legacy-test-admin".into());
    let owner = Uuid::from_u128(31_032);
    let path = format!("{}{}/{}", server.base, "/api/v1/avatar", owner);
    let original = figura_package();
    let upload = |bytes: Vec<u8>| {
        server
            .client
            .put(&path)
            .header("token", "legacy-test-admin")
            .body(bytes)
    };
    assert_eq!(upload(original.clone()).send().await.unwrap().status(), 200);
    seed_configuration(&server, owner, 7);
    let before = server.state.storage.outfit_current(owner).unwrap();
    let equipped = server.state.storage.equipped(owner).unwrap();
    server.state.storage.library_read(|database| {
        database.execute_batch("CREATE TEMP TRIGGER fail_legacy_file_publication BEFORE INSERT ON outfit_configurations
            BEGIN SELECT RAISE(ABORT, 'synthetic legacy publication failure'); END;").unwrap();
        Ok(())
    }).unwrap();
    let (replacement, _) = crate::service::package::tests::fixture(false, false);
    let response = upload(replacement).send().await.unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(server.state.storage.outfit_current(owner).unwrap(), before);
    assert_eq!(server.state.storage.equipped(owner).unwrap(), equipped);
    let retained_file =
        std::fs::read(crate::service::resource::legacy_read_path(owner, false)).unwrap();
    server
        .state
        .storage
        .library_read(|database| {
            database
                .execute_batch("DROP TRIGGER fail_legacy_file_publication;")
                .unwrap();
            Ok(())
        })
        .unwrap();
    crate::service::resource::ensure_legacy_import(&server.state, owner, false)
        .await
        .unwrap();
    let after_read = server.state.storage.equipped(owner).unwrap();
    let after_configuration = server.state.storage.outfit_current(owner).unwrap();
    std::fs::remove_file(crate::service::resource::legacy_read_path(owner, false)).unwrap();
    assert_eq!(
        retained_file, original,
        "a rejected upload must retain the prior legacy file"
    );
    assert_eq!(
        after_read, equipped,
        "reading must not activate a rejected upload"
    );
    assert_eq!(after_configuration, before);
}

#[tokio::test]
async fn opening_storage_recovers_only_uncommitted_legacy_filename_replacements() {
    let server = Server::start().await;
    server.state.config.write().await.token = Some("legacy-test-admin".into());
    let owner = Uuid::from_u128(31_033);
    let original = figura_package();
    let (replacement, _) = crate::service::package::tests::fixture(false, false);
    let upload_path = format!("{}/api/v1/avatar/{owner}", server.base);
    assert_eq!(
        server
            .client
            .put(&upload_path)
            .header("token", "legacy-test-admin")
            .body(original.clone())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let equipped = server.state.storage.equipped(owner).unwrap();
    let configuration = server.state.storage.outfit_current(owner).unwrap();

    // Persist the exact recovery state left between filename replacement and
    // SQLite commit. This tests production open/recovery, not an OS-process kill.
    for (pending_owner, had_previous) in [(owner, true), (Uuid::from_u128(31_034), false)] {
        let operation = library::random_id();
        let folder = server
            ._directory
            .path()
            .join("recycle/legacy-publications")
            .join(pending_owner.to_string())
            .join(&operation);
        std::fs::create_dir_all(&folder).unwrap();
        if had_previous {
            std::fs::write(folder.join("previous.moon"), &original).unwrap();
        }
        let target = crate::service::resource::legacy_path(pending_owner, false);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, &replacement).unwrap();
        server
            .state
            .storage
            .library_transaction(|transaction| {
                transaction
                    .execute(
                        "INSERT INTO metadata VALUES (?1, ?2)",
                        params![
                            format!("legacy_publication:{pending_owner}"),
                            json!({"operation": operation, "had_previous": had_previous})
                                .to_string()
                        ],
                    )
                    .unwrap();
                Ok(())
            })
            .unwrap();
        let reopened = Storage::open(server._directory.path()).unwrap();
        if had_previous {
            assert_eq!(std::fs::read(&target).unwrap(), original);
            assert_eq!(reopened.equipped(owner).unwrap(), equipped);
            assert_eq!(reopened.outfit_current(owner).unwrap(), configuration);
        } else {
            assert!(!target.exists());
            assert!(reopened.equipped(pending_owner).unwrap().primary.is_none());
        }
        reopened
            .library_read(|database| {
                let count: i64 = database
                    .query_row(
                        "SELECT count(*) FROM metadata WHERE key GLOB 'legacy_publication:*'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(count, 0);
                Ok(())
            })
            .unwrap();
        assert!(!folder.exists());
        drop(reopened);
    }
    assert_eq!(
        server
            .client
            .put(&upload_path)
            .header("token", "legacy-test-admin")
            .body(replacement.clone())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let committed = server.state.storage.equipped(owner).unwrap();
    let orphan = server
        ._directory
        .path()
        .join("recycle/legacy-publications")
        .join(owner.to_string())
        .join(library::random_id());
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("previous.moon"), &original).unwrap();
    let reopened = Storage::open(server._directory.path()).unwrap();
    assert_eq!(
        std::fs::read(crate::service::resource::legacy_path(owner, false)).unwrap(),
        replacement
    );
    assert_eq!(reopened.equipped(owner).unwrap(), committed);
    drop(reopened);
    // No journal means SQLite committed: an orphan cleanup file must never undo it.
    std::fs::remove_file(orphan.join("previous.moon")).unwrap();
    std::fs::remove_dir(orphan).unwrap();
    std::fs::remove_file(crate::service::resource::legacy_path(owner, false)).unwrap();
}

#[tokio::test]
async fn legacy_equip_selection_and_empty_list_normalize_the_outfit() {
    let server = Server::start().await;
    let owner = Uuid::from_u128(31_041);
    wardrobe::upload(&server, 31_041, &figura_package()).await;
    let before_equipment = server.state.storage.equipped(owner).unwrap();
    let request = || {
        server
            .client
            .post(format!("{}/api/equip", server.base))
            .header("token", "synthetic-token-31041")
    };
    let selected = json!([{"owner": owner, "id": "avatar"}]);
    seed_configuration(&server, owner, 7);
    assert_eq!(
        request().json(&selected).send().await.unwrap().status(),
        200
    );
    let enabled = server.state.storage.outfit_current(owner).unwrap();
    assert_eq!(enabled.revision, 8);
    assert!(enabled.instances.is_empty());
    assert!(enabled.base.is_some());
    assert_eq!(
        server.state.storage.equipped(owner).unwrap(),
        before_equipment
    );
    seed_configuration(&server, owner, 9);
    let preserved = server.state.storage.outfit_current(owner).unwrap();
    assert_eq!(request().send().await.unwrap().status(), 200);
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap(),
        preserved
    );
    for (body, status) in [
        (json!([{"owner": Uuid::from_u128(2), "id": "avatar"}]), 403),
        (json!([{"owner": owner, "id": "../avatar"}]), 400),
        (
            json!([{"owner": owner, "id": "avatar"}, {"owner": owner, "id": "avatar"}]),
            400,
        ),
    ] {
        assert_eq!(request().json(&body).send().await.unwrap().status(), status);
        assert_eq!(
            server.state.storage.outfit_current(owner).unwrap(),
            preserved
        );
    }
    server
        .state
        .config
        .write()
        .await
        .outfit
        .limits
        .max_json_bytes = 256;
    assert_eq!(
        request().json(&selected).send().await.unwrap().status(),
        400
    );
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap(),
        preserved
    );
    assert_eq!(
        server.state.storage.equipped(owner).unwrap(),
        before_equipment
    );
    server
        .state
        .config
        .write()
        .await
        .outfit
        .limits
        .max_json_bytes = 65536;
    assert_eq!(
        request().json(&json!([])).send().await.unwrap().status(),
        200
    );
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap(),
        OutfitConfig {
            revision: 10,
            ..Default::default()
        }
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
    crate::service::resource::ensure_legacy_import(&server.state, owner, false)
        .await
        .unwrap();
    assert!(
        server
            .state
            .storage
            .equipped(owner)
            .unwrap()
            .primary
            .is_none()
    );
    assert_eq!(
        request().json(&selected).send().await.unwrap().status(),
        200
    );
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap().revision,
        11
    );
    assert_eq!(
        server.state.storage.equipped(owner).unwrap().primary,
        before_equipment.primary
    );
    std::fs::remove_file(crate::service::resource::legacy_path(owner, false)).unwrap();
}

#[tokio::test]
async fn legacy_equip_supports_existing_native_assets_but_not_catalog_alias_bypass() {
    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    let native = outfit_resources::upload_private(&server, "legacy-native-selection").await;
    let request = || {
        server
            .client
            .post(format!("{}/api/equip", server.base))
            .header("token", "synthetic-token-1")
    };
    seed_configuration(&server, owner, 4);
    let selected = json!([{"owner": owner, "id": native.asset_id}]);
    assert_eq!(
        request().json(&selected).send().await.unwrap().status(),
        200
    );
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap().revision,
        5
    );
    let equipped = server.state.storage.equipped(owner).unwrap();
    assert_eq!(equipped.primary, Some(native));
    let (model, _) = server.upload_model("PUBLIC").await;
    assert_eq!(
        server
            .request(
                Method::POST,
                &format!("/api/library/v1/models/{model}/equip"),
                1
            )
            .json(
                &json!({"operation_id": library::random_id(), "model_revision": 1,
            "expected_equipment_revision": equipped.revision})
            )
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let source_equipment = server.state.storage.equipped(owner).unwrap();
    let source = source_equipment.primary.as_ref().unwrap();
    let before = server.state.storage.outfit_current(owner).unwrap();
    assert_eq!(
        request()
            .json(&json!([{"owner": owner, "id": source.asset_id}]))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        server.state.storage.equipped(owner).unwrap(),
        source_equipment
    );
    assert_eq!(server.state.storage.outfit_current(owner).unwrap(), before);
    assert_eq!(
        request().json(&json!([])).send().await.unwrap().status(),
        200
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
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap().revision,
        7
    );
    assert_eq!(
        server
            .state
            .storage
            .avatar(owner, &source.asset_id, source.revision)
            .unwrap(),
        *source
    );
}

#[tokio::test]
async fn legacy_equip_first_file_import_advances_the_configuration_only_once() {
    let server = Server::start().await;
    let owner = Uuid::from_u128(31_043);
    server
        .state
        .user_manager
        .insert(
            owner,
            "legacy-first-import".into(),
            Userinfo {
                uuid: owner,
                nickname: "Legacy first import".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let path = crate::service::resource::legacy_path(owner, false);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, figura_package()).unwrap();
    seed_configuration(&server, owner, 4);
    let response = server
        .client
        .post(format!("{}/api/equip", server.base))
        .header("token", "legacy-first-import")
        .json(&json!([{"owner": owner, "id": "avatar"}]))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let configuration = server.state.storage.outfit_current(owner).unwrap();
    assert_eq!(configuration.revision, 5);
    assert!(configuration.instances.is_empty());
    assert!(configuration.base.is_some());
    assert_eq!(
        server
            .state
            .storage
            .equipped(owner)
            .unwrap()
            .primary
            .unwrap()
            .revision,
        1
    );
    std::fs::remove_file(path).unwrap();
}
