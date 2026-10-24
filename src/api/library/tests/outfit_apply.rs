use super::*;
use crate::storage::outfit_apply::ApplyInput;

#[tokio::test]
async fn outfit_apply_rejects_unavailable_dependencies_and_committed_size_overflow() {
    use crate::service::outfit::{OutfitConfig, ResourceRef};
    let server = Server::start().await;
    let (public, bytes) = server.upload_model("PUBLIC").await;
    let (private, _) = server.upload_model("PRIVATE").await;
    let actor = crate::service::library_policy::Actor {
        uuid: Uuid::from_u128(2),
        admin_context: false,
        granted: false,
    };
    let limits = crate::service::outfit_limits::OutfitLimits::default();
    let reference = ResourceRef::Cloud {
        origin: server.base.clone(),
        resource_id: public.parse().unwrap(),
        revision: 1,
        content_hash: faster_hex::hex_string(&crate::storage::sha256(&bytes)),
        format: "FIGURA".into(),
    };
    for (field, value, expected) in [
        ("resource_id", private.as_str(), ErrorCode::NotFound),
        ("content_hash", &"0".repeat(64), ErrorCode::Conflict),
        ("format", "YSM_PLAIN", ErrorCode::Conflict),
        (
            "origin",
            "https://foreign.example",
            ErrorCode::UnsupportedCapability,
        ),
    ] {
        let mut changed = serde_json::to_value(&reference).unwrap();
        changed[field] = json!(value);
        let mut configuration = OutfitConfig::default();
        configuration.base = Some(serde_json::from_value(changed).unwrap());
        let rejected =
            server
                .state
                .storage
                .outfit_apply(actor, &server.base, &input(configuration), &limits);
        assert_eq!(rejected.unwrap_err(), expected);
        assert_eq!(
            server
                .state
                .storage
                .outfit_current(actor.uuid)
                .unwrap()
                .revision,
            0
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
    let mut tight = limits;
    tight.max_json_bytes = 256;
    let configuration = (1..512)
        .find_map(|length| {
            let mut value = OutfitConfig::default();
            value.revision = 9;
            value.display_ordinals.insert("x".repeat(length), 1);
            (serde_json::to_vec(&value).unwrap().len() == 256).then_some(value)
        })
        .unwrap();
    server
        .state
        .storage
        .library_read(|database| {
            database
                .execute(
                    "INSERT INTO outfit_configurations VALUES (?1, 9, ?2)",
                    rusqlite::params![
                        actor.uuid.to_string(),
                        serde_json::to_string(&configuration).unwrap()
                    ],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &input(configuration.clone()), &tight)
            .unwrap_err(),
        ErrorCode::InvalidMessage,
        "revision 9 to 10 must not publish JSON above the negotiated byte budget"
    );
    assert_eq!(
        server.state.storage.outfit_current(actor.uuid).unwrap(),
        configuration
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

fn input(configuration: crate::service::outfit::OutfitConfig) -> ApplyInput {
    ApplyInput {
        operation_id: library::random_id().parse().unwrap(),
        expected_revision: configuration.revision,
        configuration,
    }
}

#[tokio::test]
async fn concurrent_outfit_apply_serializes_cas_and_identical_retries() {
    let server = Server::start().await;
    for same_operation in [false, true] {
        let actor = crate::service::library_policy::Actor {
            uuid: Uuid::from_u128(if same_operation { 3 } else { 2 }),
            admin_context: false,
            granted: false,
        };
        let first = input(crate::service::outfit::OutfitConfig::default());
        let second = if same_operation {
            first.clone()
        } else {
            input(crate::service::outfit::OutfitConfig::default())
        };
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let mut jobs = Vec::new();
        for request in [first, second] {
            let ready = barrier.clone();
            let storage = server.state.storage.clone();
            let origin = server.base.clone();
            jobs.push(tokio::spawn(async move {
                ready.wait().await;
                tokio::task::spawn_blocking(move || {
                    storage.outfit_apply(
                        actor,
                        &origin,
                        &request,
                        &crate::service::outfit_limits::OutfitLimits::default(),
                    )
                })
                .await
                .unwrap()
            }));
        }
        let results = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let left = jobs.remove(0).await.unwrap();
            let right = jobs.remove(0).await.unwrap();
            [left, right]
        })
        .await
        .expect("bounded concurrent Apply");
        if same_operation {
            assert_eq!(results[0].as_ref().unwrap(), results[1].as_ref().unwrap());
        } else {
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            assert!(
                results
                    .iter()
                    .any(|result| matches!(result, Err(ErrorCode::Conflict)))
            );
        }
        assert_eq!(
            server
                .state
                .storage
                .outfit_current(actor.uuid)
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            server.state.storage.equipped(actor.uuid).unwrap().revision,
            0
        );
    }
}

#[tokio::test]
async fn outfit_apply_updates_projection_atomically_and_replays() {
    let server = Server::start().await;
    let (first, first_bytes) = server.upload_model("PUBLIC").await;
    let (second, second_bytes) = server.upload_model("PUBLIC").await;
    let actor = crate::service::library_policy::Actor {
        uuid: Uuid::from_u128(2),
        admin_context: false,
        granted: false,
    };
    let limits = crate::service::outfit_limits::OutfitLimits::default();
    let reference = |id: &str, bytes: &[u8]| crate::service::outfit::ResourceRef::Cloud {
        origin: server.base.clone(),
        resource_id: Uuid::parse_str(id).unwrap(),
        revision: 1,
        content_hash: faster_hex::hex_string(&crate::storage::sha256(bytes)),
        format: "FIGURA".into(),
    };
    let mut configuration = crate::service::outfit::OutfitConfig::default();
    configuration.base = Some(reference(&first, &first_bytes));
    let first_input = input(configuration);
    let applied = server
        .state
        .storage
        .outfit_apply(actor, &server.base, &first_input, &limits)
        .unwrap();
    assert_eq!(applied.configuration.revision, 1);
    let equipment = server.state.storage.equipped(actor.uuid).unwrap();
    assert_eq!(
        equipment.primary.as_ref().unwrap().raw_sha256,
        crate::storage::sha256(&first_bytes)
    );
    assert_eq!(equipment.revision, applied.equipment_revision);
    let mut updated = input(applied.configuration.clone());
    updated.configuration.show_equipment = true;
    let changed = server
        .state
        .storage
        .outfit_apply(actor, &server.base, &updated, &limits)
        .unwrap();
    assert_eq!(changed.configuration.revision, 2);
    assert_eq!(
        changed.equipment_revision, applied.equipment_revision,
        "configuration changes preserve the base generation"
    );
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &first_input, &limits)
            .unwrap(),
        applied
    );
    assert_eq!(
        server.state.storage.outfit_current(actor.uuid).unwrap(),
        changed.configuration,
        "replaying an old operation returns its result without reverting a newer configuration"
    );
    let mut stale = first_input.clone();
    stale.operation_id = library::random_id().parse().unwrap();
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &stale, &limits)
            .unwrap_err(),
        ErrorCode::Conflict
    );
    let mut conflicting = first_input.clone();
    conflicting.configuration.show_equipment = true;
    assert_eq!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &conflicting, &limits)
            .unwrap_err(),
        ErrorCode::Conflict
    );
    let mut replacement = input(changed.configuration.clone());
    replacement.configuration.base = Some(reference(&second, &second_bytes));
    server
        .state
        .storage
        .library_read(|database| {
            database
                .execute_batch(
                    "CREATE TEMP TRIGGER reject_outfit_apply BEFORE INSERT ON outfit_configurations
            BEGIN SELECT RAISE(ABORT, 'injected configuration write failure'); END;",
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    assert!(
        server
            .state
            .storage
            .outfit_apply(actor, &server.base, &replacement, &limits)
            .is_err()
    );
    assert_eq!(
        server.state.storage.equipped(actor.uuid).unwrap(),
        equipment,
        "failed SQL publication cannot expose the candidate legacy base projection"
    );
    assert_eq!(
        server.state.storage.outfit_current(actor.uuid).unwrap(),
        changed.configuration
    );
    server.state.storage.library_read(|database| {
        let aliases: i64 = database.query_row(
            "SELECT count(*) FROM library_resource_bindings WHERE resource_owner = ?1 AND source_model_id = ?2",
            rusqlite::params![actor.uuid.to_string(), second], |row| row.get(0)).unwrap();
        assert_eq!(aliases, 0, "source alias bindings roll back with the configuration");
        database.execute_batch("DROP TRIGGER reject_outfit_apply").unwrap();
        Ok(())
    }).unwrap();
    let replaced = server
        .state
        .storage
        .outfit_apply(actor, &server.base, &replacement, &limits)
        .unwrap();
    assert_eq!(replaced.configuration.revision, 3);
    assert_eq!(replaced.equipment_revision, equipment.revision + 1);
    let mut vanilla = input(replaced.configuration);
    vanilla.configuration.base = None;
    let cleared = server
        .state
        .storage
        .outfit_apply(actor, &server.base, &vanilla, &limits)
        .unwrap();
    assert_eq!(cleared.configuration.revision, 4);
    let equipment = server.state.storage.equipped(actor.uuid).unwrap();
    assert!(equipment.primary.is_none() && equipment.figura_compatibility.is_none());
    let capabilities: Value = server
        .request(Method::GET, "/api/library/v1/capabilities", 2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        capabilities["outfits_v1"]["apply"], true,
        "the stage-five public flow is advertised"
    );
}
