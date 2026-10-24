use super::*;
use crate::{
    protocol::v1::CommitUpload,
    service::{
        outfit::{OutfitConfig, ResourceRef},
        outfit_limits::OutfitLimits,
        resource,
    },
    storage::{outfit_apply::ApplyInput, outfit_uploads::PrivateUpload},
};

const UPLOADS: &str = "/api/library/v1/outfits/resources/uploads";

pub(super) fn input(bytes: &[u8]) -> PrivateUpload {
    PrivateUpload {
        operation_id: library::random_id().parse().unwrap(),
        reference: ResourceRef::Local {
            local_id: library::random_id().parse().unwrap(),
            content_hash: "e".repeat(64),
            format: "FIGURA".into(),
        },
        resource_kind: crate::service::outfit::ResourceKind::Avatar,
        accessory_metadata: None,
        raw_sha256: faster_hex::hex_string(&crate::storage::sha256(bytes)),
        byte_length: bytes.len() as u64,
        content_sha256: None,
        schema_sha256: None,
    }
}

pub(super) async fn reserve(server: &Server, input: &PrivateUpload) -> Value {
    let response = server
        .request(Method::POST, UPLOADS, 1)
        .json(input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    response.json().await.unwrap()
}

#[tokio::test]
async fn outfit_private_upload_is_atomic_private_on_first_commit_and_idempotent() {
    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    let bytes = figura_package();
    let input = input(&bytes);
    assert_eq!(
        server
            .client
            .post(format!("{}{UPLOADS}", server.base))
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let reserved = reserve(&server, &input).await;
    assert_eq!(reserved["completed"], false);
    let path = reserved["relative_path"].as_str().unwrap();
    let commit = format!("{UPLOADS}/{}/commit", input.operation_id);
    let descriptor = input.request(owner).unwrap().resource.unwrap();
    assert!(
        server
            .state
            .storage
            .avatar(owner, &descriptor.asset_id, 1)
            .is_err()
    );
    assert_eq!(
        server
            .request(Method::PUT, path, 2)
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .request(Method::POST, &commit, 2)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .request(Method::PUT, path, 1)
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    server.state.storage.library_transaction(|database| {
        database.execute_batch("CREATE TEMP TRIGGER fail_private_first_commit BEFORE INSERT ON outfit_private_sources
            BEGIN SELECT RAISE(ABORT, 'simulated first-commit marker failure'); END;").unwrap();
        Ok(())
    }).unwrap();
    assert_eq!(
        server
            .request(Method::POST, &commit, 1)
            .send()
            .await
            .unwrap()
            .status(),
        500
    );
    assert_eq!(
        server
            .state
            .storage
            .avatar(owner, &descriptor.asset_id, 1)
            .unwrap_err(),
        ErrorCode::NotFound
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
            assert_eq!(count, 0);
            database
                .execute_batch("DROP TRIGGER fail_private_first_commit;")
                .unwrap();
            Ok(())
        })
        .unwrap();
    let response = server
        .request(Method::POST, &commit, 1)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let binding: Value = response.json().await.unwrap();
    assert_eq!(
        binding["reference"],
        serde_json::to_value(&input.reference).unwrap()
    );
    assert_eq!(binding["asset_revision"], 1);
    let observer = crate::service::library_policy::Actor {
        uuid: Uuid::from_u128(2),
        admin_context: false,
        granted: false,
    };
    assert_eq!(
        server
            .state
            .storage
            .library_resource_access(owner, &descriptor.asset_id, 1, observer)
            .unwrap_err(),
        ErrorCode::NotFound,
        "the first committed revision must already be private"
    );
    let native = server
        .state
        .storage
        .avatar(owner, &descriptor.asset_id, 1)
        .unwrap();
    assert_eq!(
        outfit_read_access::download(&server, &native, 2)
            .await
            .status(),
        404
    );
    server
        .state
        .storage
        .library_read(|database| {
            let native: i64 = database
                .query_row("SELECT count(*) FROM avatars", [], |row| row.get(0))
                .unwrap();
            let marked: i64 = database
                .query_row("SELECT count(*) FROM outfit_private_sources", [], |row| {
                    row.get(0)
                })
                .unwrap();
            let catalog: i64 = database
                .query_row("SELECT count(*) FROM library_models", [], |row| row.get(0))
                .unwrap();
            assert_eq!((native, marked, catalog), (1, 1, 0));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap(),
        OutfitConfig::default()
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
    resource::grant_upload(&server.state, owner, &input.request(owner).unwrap())
        .await
        .unwrap();
    resource::commit_upload(
        &server.state,
        owner,
        &CommitUpload {
            operation_id: input.operation_id.to_string(),
            raw_sha256: descriptor.raw_sha256.clone(),
            raw_length: descriptor.raw_length,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(
            resource::begin_upload(&server.state, owner, input.operation_id),
            Err(ErrorCode::NotFound)
        ),
        "a completed private replay must release its redundant native grant"
    );
    assert_eq!(reserve(&server, &input).await["resource"], binding);
    assert_eq!(
        server
            .request(Method::POST, &commit, 1)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        binding
    );
    let mut changed = input.clone();
    changed.raw_sha256 = "f".repeat(64);
    assert_eq!(
        server
            .request(Method::POST, UPLOADS, 1)
            .json(&changed)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let mut configuration = OutfitConfig::default();
    configuration.base = Some(input.reference.clone());
    configuration.show_equipment = true;
    server
        .state
        .storage
        .outfit_apply(
            crate::service::library_policy::Actor {
                uuid: owner,
                admin_context: false,
                granted: false,
            },
            &server.base,
            &ApplyInput {
                operation_id: library::random_id().parse().unwrap(),
                expected_revision: 0,
                configuration,
            },
            &OutfitLimits::default(),
        )
        .unwrap();
    server
        .state
        .storage
        .library_resource_access(owner, &descriptor.asset_id, 1, observer)
        .unwrap();
    // Durable source preparation also survives expired/removed generic operation receipts.
    server
        .state
        .storage
        .library_transaction(|database| {
            database
                .execute(
                    "DELETE FROM operations WHERE id = ?1",
                    [format!("{owner}/{}", input.operation_id)],
                )
                .unwrap();
            database
                .execute(
                    "DELETE FROM library_operations WHERE actor_uuid = ?1 AND operation_id = ?2",
                    params![owner.to_string(), input.operation_id.to_string()],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    let reopened = Storage::open(server._directory.path()).unwrap();
    assert_eq!(
        reopened
            .outfit_reserve_upload(owner, &input, &OutfitLimits::default(), 10)
            .unwrap(),
        input
    );
    assert_eq!(
        reopened
            .outfit_completed_upload(owner, input.operation_id, &OutfitLimits::default())
            .unwrap()
            .unwrap()
            .revision,
        1
    );
}

async fn upload_private(server: &Server, input: &PrivateUpload, bytes: &[u8]) -> reqwest::Response {
    let reserved = reserve(server, input).await;
    let path = reserved["relative_path"].as_str().unwrap().to_owned();
    assert_eq!(
        server
            .request(Method::PUT, &path, 1)
            .header("content-type", "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    server
        .request(
            Method::POST,
            &format!("{UPLOADS}/{}/commit", input.operation_id),
            1,
        )
        .send()
        .await
        .unwrap()
}

/// A source packaged by an older client can be replaced by the owner's current packaging of the same source.
#[tokio::test]
async fn outfit_private_upload_replaces_an_outdated_package_of_the_same_local_source() {
    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    let outdated = figura_package_named("Outdated packaging");
    let first = input(&outdated);
    let response = upload_private(&server, &first, &outdated).await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());

    let current = figura_package_named("Current packaging");
    let mut second = input(&current);
    second.reference = first.reference.clone();
    let response = upload_private(&server, &second, &current).await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let binding: Value = response.json().await.unwrap();
    assert_eq!(binding["raw_sha256"], second.raw_sha256);

    let resolved = server
        .state
        .storage
        .outfit_resolve_local(
            owner,
            &first.reference,
            crate::service::outfit::ResourceKind::Avatar,
            &OutfitLimits::default(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(resolved.raw_sha256, second.raw_sha256);
    assert_eq!(
        resolved.asset_id,
        second.request(owner).unwrap().resource.unwrap().asset_id
    );
}

#[tokio::test]
async fn outfit_private_reservation_blocks_native_operation_and_asset_repurposing() {
    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    let bytes = figura_package();
    let input = input(&bytes);
    let expected = input.request(owner).unwrap();
    // Admit an ordinary upload before the private reservation exists.
    let mut earlier = expected.clone();
    earlier.operation_id = library::random_id();
    resource::grant_upload(&server.state, owner, &earlier)
        .await
        .unwrap();
    reserve(&server, &input).await;
    let mut altered = expected.clone();
    altered.resource.as_mut().unwrap().raw_sha256 = vec![0; 32];
    assert_eq!(
        resource::grant_upload(&server.state, owner, &altered)
            .await
            .unwrap_err(),
        ErrorCode::Conflict
    );
    assert_eq!(
        resource::grant_upload(&server.state, owner, &earlier)
            .await
            .unwrap_err(),
        ErrorCode::Conflict
    );
    let mut upload =
        resource::begin_upload(&server.state, owner, earlier.operation_id.parse().unwrap())
            .unwrap();
    resource::receive_upload(
        &mut upload,
        axum::body::Body::from(bytes.clone()),
        || Ok(()),
    )
    .await
    .unwrap();
    drop(upload);
    assert_eq!(
        resource::commit_upload(
            &server.state,
            owner,
            &CommitUpload {
                operation_id: earlier.operation_id,
                raw_sha256: crate::storage::sha256(&bytes),
                raw_length: bytes.len() as u64,
            }
        )
        .await
        .unwrap_err(),
        ErrorCode::Conflict
    );
    assert_eq!(
        server
            .state
            .storage
            .avatar(owner, &expected.resource.unwrap().asset_id, 1)
            .unwrap_err(),
        ErrorCode::NotFound
    );
}

#[tokio::test]
async fn outfit_private_java_figura_and_encrypted_accessories_use_native_commit_without_catalog_publication()
 {
    let Ok(directory) = std::env::var("OMNIMORPH_TEST_ACCESSORY_UPLOAD_DIRECTORY") else {
        eprintln!(
            "SKIPPED private Java accessory upload: set OMNIMORPH_TEST_ACCESSORY_UPLOAD_DIRECTORY"
        );
        return;
    };
    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    for family in ["figura", "encrypted"] {
        let directory = std::path::Path::new(&directory).join(family);
        let bytes = std::fs::read(directory.join("body.nbt")).unwrap();
        let fixture: Value =
            serde_json::from_slice(&std::fs::read(directory.join("reserve.json")).unwrap())
                .unwrap();
        let input: PrivateUpload = serde_json::from_value(json!({
            "operation_id": library::random_id(),
            "reference": {"source": "LOCAL", "local_id": library::random_id(), "content_hash": "f".repeat(64), "format": fixture["metadata"]["model_type"]},
            "resource_kind": "ACCESSORY", "accessory_metadata": fixture["metadata"]["accessory_metadata"],
            "raw_sha256": fixture["raw_sha256"], "byte_length": fixture["byte_length"],
            "content_sha256": fixture["content_sha256"], "schema_sha256": fixture["schema_sha256"]
        })).unwrap();
        let reserved = reserve(&server, &input).await;
        let response = server
            .request(Method::PUT, reserved["relative_path"].as_str().unwrap(), 1)
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204, "{}", response.text().await.unwrap());
        // The unchanged native entry point must apply the durable private purpose too.
        let descriptor = input.request(owner).unwrap().resource.unwrap();
        server
            .state
            .config
            .write()
            .await
            .outfit
            .limits
            .position_limit = 0.5;
        assert_eq!(
            resource::commit_upload(
                &server.state,
                owner,
                &CommitUpload {
                    operation_id: input.operation_id.to_string(),
                    raw_sha256: descriptor.raw_sha256.clone(),
                    raw_length: descriptor.raw_length,
                }
            )
            .await
            .unwrap_err(),
            ErrorCode::InvalidMessage,
            "native commit must recheck current author budgets too"
        );
        assert_eq!(
            server
                .state
                .storage
                .avatar(owner, &descriptor.asset_id, 1)
                .unwrap_err(),
            ErrorCode::NotFound
        );
        server
            .state
            .config
            .write()
            .await
            .outfit
            .limits
            .position_limit = OutfitLimits::default().position_limit;
        let committed = resource::commit_upload(
            &server.state,
            owner,
            &CommitUpload {
                operation_id: input.operation_id.to_string(),
                raw_sha256: descriptor.raw_sha256.clone(),
                raw_length: descriptor.raw_length,
            },
        )
        .await
        .unwrap();
        assert_eq!(committed.revision, 1);
        assert_eq!(committed.raw_sha256, crate::storage::sha256(&bytes));
        let binding = reserve(&server, &input).await;
        assert_eq!(binding["completed"], true);
        assert_eq!(binding["resource"]["resource_kind"], "ACCESSORY");
        assert_eq!(
            binding["resource"]["accessory_metadata"]["validation_source"],
            "CLIENT_EXTRACTED"
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
    server
        .state
        .storage
        .library_read(|database| {
            let count: i64 = database
                .query_row("SELECT count(*) FROM library_models", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0);
            Ok(())
        })
        .unwrap();
}
