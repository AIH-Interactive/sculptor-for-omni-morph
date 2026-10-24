use super::*;
use crate::{service::outfit::OutfitConfig, storage::outfit_apply::ApplyInput};

const CURRENT: &str = "/api/library/v1/outfits/current";

fn input(configuration: OutfitConfig) -> ApplyInput {
    ApplyInput {
        operation_id: library::random_id().parse().unwrap(),
        expected_revision: configuration.revision,
        configuration,
    }
}

async fn apply(server: &Server, input: &ApplyInput, player: u128) -> reqwest::Response {
    server
        .request(Method::PUT, CURRENT, player)
        .header("x-omnimorph-origin", &server.base)
        .json(input)
        .send()
        .await
        .unwrap()
}

fn changed(
    events: &mut tokio::sync::broadcast::Receiver<crate::service::event::CloudEvent>,
    owner: Uuid,
) {
    assert!(
        matches!(events.try_recv(), Ok(crate::service::event::CloudEvent::AvatarChanged { owner: actual, tombstone: false }) if actual == owner)
    );
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn outfit_http_apply_replays_old_receipts_without_republishing_and_returns_current_conflicts()
{
    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    let mut events = server.state.subscribes.subscribe(owner);
    let first = input(OutfitConfig::default());
    assert_eq!(
        server
            .client
            .put(format!("{}{CURRENT}", server.base))
            .json(&first)
            .header("x-omnimorph-origin", &server.base)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        server
            .request(Method::PUT, CURRENT, 1)
            .json(&first)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let response = apply(&server, &first, 1).await;
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let original: Value = response.json().await.unwrap();
    assert_eq!(original["schema_version"], 1);
    assert_eq!(original["operation_id"], first.operation_id.to_string());
    assert_eq!(original["applied"]["configuration"]["revision"], 1);
    assert_eq!(original["current_revision"], 1);
    changed(&mut events, owner);
    assert_eq!(
        apply(&server, &first, 1)
            .await
            .json::<Value>()
            .await
            .unwrap(),
        original
    );
    assert!(events.try_recv().is_err());
    let mut next: OutfitConfig =
        serde_json::from_value(original["applied"]["configuration"].clone()).unwrap();
    next.show_equipment = true;
    let second = input(next);
    let second = apply(&server, &second, 1)
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(second["current_revision"], 2);
    changed(&mut events, owner);
    let replay = apply(&server, &first, 1)
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(replay["applied"], original["applied"]);
    assert_eq!(
        replay["current_revision"], 2,
        "old acknowledgement cannot authorize activating revision 1"
    );
    assert!(events.try_recv().is_err());
    let stale = input(first.configuration.clone());
    let response = apply(&server, &stale, 1).await;
    assert_eq!(response.status(), 409);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let conflict: Value = response.json().await.unwrap();
    assert_eq!(conflict["code"], "CONFLICT");
    assert_eq!(conflict["current_revision"], 2);
    assert_eq!(
        conflict["configuration"],
        second["applied"]["configuration"]
    );
    assert_eq!(conflict["request_id"], request_id);
    assert_eq!(conflict["retryable"], false);
    let mut altered = first.clone();
    altered.configuration.show_equipment = true;
    assert_eq!(apply(&server, &altered, 1).await.status(), 409);
    let mut injected = serde_json::to_value(input(OutfitConfig::default())).unwrap();
    injected["owner"] = json!(owner);
    assert_eq!(
        server
            .request(Method::PUT, CURRENT, 2)
            .header("x-omnimorph-origin", &server.base)
            .json(&injected)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap().revision,
        2
    );
    assert_eq!(
        server
            .state
            .storage
            .outfit_current(Uuid::from_u128(2))
            .unwrap()
            .revision,
        0
    );
    assert!(events.try_recv().is_err());
    let capabilities: Value = server
        .request(Method::GET, "/api/library/v1/capabilities", 1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(capabilities["outfits_v1"]["apply"], true);
    assert_eq!(capabilities["outfits_v1"]["events"], true);
}

#[tokio::test]
async fn outfit_http_apply_native_base_rolls_back_on_failure_and_never_uses_another_players_binding()
 {
    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    let bytes = figura_package();
    let source = outfit_uploads::input(&bytes);
    let reserved = outfit_uploads::reserve(&server, &source).await;
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
                    source.operation_id
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
    configuration.base = Some(source.reference);
    configuration.show_equipment = true;
    let request = input(configuration);
    let mut events = server.state.subscribes.subscribe(owner);
    server.state.storage.library_transaction(|database| {
        database.execute_batch("CREATE TEMP TRIGGER fail_public_outfit_apply BEFORE INSERT ON outfit_configurations
            BEGIN SELECT RAISE(ABORT, 'simulated public Apply failure'); END;").unwrap();
        Ok(())
    }).unwrap();
    assert_eq!(apply(&server, &request, 1).await.status(), 500);
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
    assert!(events.try_recv().is_err());
    server
        .state
        .storage
        .library_transaction(|database| {
            database
                .execute_batch("DROP TRIGGER fail_public_outfit_apply;")
                .unwrap();
            Ok(())
        })
        .unwrap();
    let result = apply(&server, &request, 1)
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(result["applied"]["equipment_revision"], 1);
    assert_eq!(result["current_revision"], 1);
    changed(&mut events, owner);
    let actual = server
        .state
        .storage
        .equipped(owner)
        .unwrap()
        .primary
        .unwrap();
    assert_eq!(
        actual.asset_id,
        format!("outfit_{}", source.operation_id.simple())
    );
    assert_eq!(actual.revision, 1);
    assert_eq!(apply(&server, &request, 2).await.status(), 404);
    assert_eq!(
        server
            .state
            .storage
            .outfit_current(Uuid::from_u128(2))
            .unwrap(),
        OutfitConfig::default()
    );
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn outfit_http_apply_concurrency_publishes_once_for_cas_and_identical_operations() {
    for same_operation in [false, true] {
        let server = Server::start().await;
        let mut events = server.state.subscribes.subscribe(Uuid::from_u128(1));
        let first = input(OutfitConfig::default());
        let second = if same_operation {
            first.clone()
        } else {
            input(OutfitConfig::default())
        };
        let (first, second) = tokio::join!(apply(&server, &first, 1), apply(&server, &second, 1));
        let statuses = [first.status().as_u16(), second.status().as_u16()];
        assert_eq!(
            statuses.iter().filter(|status| **status == 200).count(),
            if same_operation { 2 } else { 1 }
        );
        assert_eq!(
            statuses.iter().filter(|status| **status == 409).count(),
            if same_operation { 0 } else { 1 }
        );
        assert_eq!(
            server
                .state
                .storage
                .outfit_current(Uuid::from_u128(1))
                .unwrap()
                .revision,
            1
        );
        changed(&mut events, Uuid::from_u128(1));
    }
}

#[tokio::test]
async fn outfit_http_apply_request_receipt_and_conflict_envelopes_have_their_own_bounded_json_space()
 {
    let server = Server::start().await;
    let configuration = (1..=507)
        .filter_map(|width| {
            let mut candidate = OutfitConfig::default();
            for index in 0..128 {
                candidate
                    .display_ordinals
                    .insert(format!("{index:03}-{}", "x".repeat(width)), 1);
            }
            candidate.validate().is_ok().then_some(candidate)
        })
        .last()
        .unwrap();
    assert!(serde_json::to_vec(&configuration).unwrap().len() > 65000);
    let request = input(configuration);
    let mut encoded = vec![b' '; 1000];
    encoded.extend_from_slice(&serde_json::to_vec(&request).unwrap());
    assert!(encoded.len() > 65536);
    let response = server
        .request(Method::PUT, CURRENT, 1)
        .header("x-omnimorph-origin", &server.base)
        .header("content-type", "application/json")
        .body(encoded)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let bytes = response.bytes().await.unwrap();
    assert!(bytes.len() > 65536);
    let receipt: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(receipt["current_revision"], 1);
    let conflict = apply(&server, &input(request.configuration.clone()), 1).await;
    assert_eq!(conflict.status(), 409);
    let conflict = conflict.bytes().await.unwrap();
    assert!(conflict.len() > 65536);
    let conflict: Value = serde_json::from_slice(&conflict).unwrap();
    assert_eq!(
        conflict["configuration"],
        receipt["applied"]["configuration"]
    );
    let mut excessive: OutfitConfig =
        serde_json::from_value(receipt["applied"]["configuration"].clone()).unwrap();
    excessive.display_ordinals.insert("z".repeat(512), 1);
    assert_eq!(apply(&server, &input(excessive), 1).await.status(), 400);
    assert_eq!(
        server
            .state
            .storage
            .outfit_current(Uuid::from_u128(1))
            .unwrap()
            .revision,
        1
    );
    let excessive_body = format!(
        "{}{{}}",
        " ".repeat(crate::api::library::outfits::APPLY_JSON_MAX_BYTES)
    );
    assert_eq!(
        server
            .request(Method::PUT, CURRENT, 1)
            .header("x-omnimorph-origin", &server.base)
            .header("content-type", "application/json")
            .body(excessive_body)
            .send()
            .await
            .unwrap()
            .status(),
        413
    );
}
