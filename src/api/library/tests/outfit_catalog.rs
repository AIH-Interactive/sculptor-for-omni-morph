use super::*;
use crate::service::outfit::ResourceRef;

fn input(revision: u64) -> Value {
    json!({"operation_id": library::random_id(), "model_revision": 1,
        "expected_equipment_revision": revision})
}

#[tokio::test]
async fn catalog_equip_normalizes_atomically_and_replays_without_overwriting_newer_outfits() {
    let server = Server::start().await;
    let owner = Uuid::from_u128(2);
    let (first, bytes) = server.upload_model("PUBLIC").await;
    let (second, _) = server.upload_model("PUBLIC").await;
    let path = format!("/api/library/v1/models/{first}/equip");
    let request = |body: &Value| {
        server
            .request(Method::POST, &path, 2)
            .header("x-omnimorph-origin", &server.base)
            .json(body)
    };
    outfit_legacy::seed_configuration(&server, owner, 7);
    let before = server.state.storage.outfit_current(owner).unwrap();
    let first_input = input(0);
    server
        .state
        .storage
        .library_read(|database| {
            database
                .execute_batch(
                    "CREATE TEMP TRIGGER fail_catalog_outfit BEFORE INSERT ON outfit_configurations
            BEGIN SELECT RAISE(ABORT, 'synthetic catalog normalization failure'); END;",
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    assert_eq!(request(&first_input).send().await.unwrap().status(), 500);
    assert_eq!(server.state.storage.outfit_current(owner).unwrap(), before);
    assert!(
        server
            .state
            .storage
            .equipped(owner)
            .unwrap()
            .primary
            .is_none()
    );
    let alias = format!("lib_{}_1", first.replace('-', ""));
    assert_eq!(
        server.state.storage.avatar(owner, &alias, 1).unwrap_err(),
        ErrorCode::NotFound
    );
    server
        .state
        .storage
        .library_read(|database| {
            let bound: i64 = database
                .query_row(
                    "SELECT count(*) FROM library_resource_bindings
            WHERE resource_owner = ?1",
                    [owner.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(bound, 0);
            database
                .execute_batch("DROP TRIGGER fail_catalog_outfit;")
                .unwrap();
            Ok(())
        })
        .unwrap();
    let response = request(&first_input).send().await.unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let first_result: Value = response.json().await.unwrap();
    let normalized = server.state.storage.outfit_current(owner).unwrap();
    assert_eq!(normalized.revision, 8);
    assert!(normalized.instances.is_empty());
    assert!(normalized.virtual_anchors.is_empty());
    assert!(normalized.display_ordinals.is_empty());
    assert!(normalized.show_equipment);
    assert_eq!(
        normalized.base,
        Some(ResourceRef::Cloud {
            origin: server.base.clone(),
            resource_id: first.parse().unwrap(),
            revision: 1,
            content_hash: faster_hex::hex_string(&crate::storage::sha256(&bytes)),
            format: "FIGURA".into(),
        })
    );
    assert_eq!(
        request(&first_input)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        first_result
    );
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap(),
        normalized
    );
    let mut hidden = input(first_result["equipment_revision"].as_u64().unwrap());
    hidden["show_equipment"] = json!(false);
    let hidden_result: Value = request(&hidden).send().await.unwrap().json().await.unwrap();
    let hidden_outfit = server.state.storage.outfit_current(owner).unwrap();
    assert_eq!(hidden_outfit.revision, 9);
    assert!(!hidden_outfit.show_equipment);
    assert_eq!(hidden_outfit.base, normalized.base);
    assert_eq!(
        request(&first_input)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        first_result
    );
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap(),
        hidden_outfit
    );
    let second_input = input(hidden_result["equipment_revision"].as_u64().unwrap());
    let response = server
        .request(
            Method::POST,
            &format!("/api/library/v1/models/{second}/equip"),
            2,
        )
        .json(&second_input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let second_result: Value = response.json().await.unwrap();
    let replaced = server.state.storage.outfit_current(owner).unwrap();
    assert_eq!(replaced.revision, 10);
    assert!(replaced.show_equipment);
    assert!(
        matches!(replaced.base.as_ref(), Some(ResourceRef::Cloud { resource_id, origin, .. })
        if resource_id.to_string() == second && origin == &server.base)
    );
    assert_eq!(request(&input(0)).send().await.unwrap().status(), 409);
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap(),
        replaced
    );
    server
        .state
        .config
        .write()
        .await
        .outfit
        .limits
        .max_json_bytes = 256;
    assert_eq!(
        request(&input(
            second_result["equipment_revision"].as_u64().unwrap()
        ))
        .send()
        .await
        .unwrap()
        .status(),
        400
    );
    assert_eq!(
        server.state.storage.outfit_current(owner).unwrap(),
        replaced
    );
    assert_eq!(
        server
            .state
            .storage
            .equipped(owner)
            .unwrap()
            .primary
            .unwrap()
            .asset_id,
        format!("lib_{}_1", second.replace('-', ""))
    );
}

#[tokio::test]
async fn catalog_origin_supports_legacy_proxy_metadata_and_validates_explicit_namespaces() {
    let server = Server::start().await;
    let owner = Uuid::from_u128(2);
    let (id, _) = server.upload_model("PUBLIC").await;
    let path = format!("/api/library/v1/models/{id}/equip");
    let request = || server.request(Method::POST, &path, 2);
    for (name, value, status) in [
        ("x-omnimorph-origin", "https://foreign.invalid", 403),
        ("x-omnimorph-origin", "not-an-origin", 400),
        ("x-forwarded-proto", "ftp", 400),
        ("x-forwarded-proto", "https,http", 400),
    ] {
        assert_eq!(
            request()
                .header(name, value)
                .json(&input(0))
                .send()
                .await
                .unwrap()
                .status(),
            status
        );
        assert_eq!(
            server.state.storage.outfit_current(owner).unwrap().revision,
            0
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
    assert_eq!(
        request()
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-proto", "http")
            .json(&input(0))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let first = input(0);
    let response = request()
        .header("x-forwarded-proto", "https")
        .json(&first)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let result: Value = response.json().await.unwrap();
    let proxied = server.state.storage.outfit_current(owner).unwrap();
    assert!(
        matches!(proxied.base.as_ref(), Some(ResourceRef::Cloud { origin, .. })
        if origin == &server.base.replacen("http:", "https:", 1))
    );
    assert_eq!(
        request()
            .header("x-omnimorph-origin", &server.base)
            .json(&first)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        result
    );
    assert_eq!(server.state.storage.outfit_current(owner).unwrap(), proxied);
    assert_eq!(
        request()
            .header("x-omnimorph-origin", &server.base)
            .header("x-forwarded-proto", "invalid")
            .json(&input(result["equipment_revision"].as_u64().unwrap()))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        matches!(server.state.storage.outfit_current(owner).unwrap().base,
        Some(ResourceRef::Cloud { origin, .. }) if origin == server.base)
    );
    assert_eq!(
        server
            .request(Method::POST, &path, 3)
            .json(&input(0))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        matches!(server.state.storage.outfit_current(Uuid::from_u128(3)).unwrap().base,
        Some(ResourceRef::Cloud { origin, .. }) if origin == server.base)
    );

    let (private, _) = server.upload_model("PRIVATE").await;
    let private_path = format!("/api/library/v1/models/{private}/equip");
    let first = server
        .request(Method::POST, &private_path, 1)
        .json(&input(0))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let first: Value = first.json().await.unwrap();
    assert!(
        !server
            .state
            .storage
            .outfit_current(Uuid::from_u128(1))
            .unwrap()
            .show_equipment
    );
    let mut visible = input(first["equipment_revision"].as_u64().unwrap());
    visible["show_private_equipment"] = json!(true);
    assert_eq!(
        server
            .request(Method::POST, &private_path, 1)
            .json(&visible)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        server
            .state
            .storage
            .outfit_current(Uuid::from_u128(1))
            .unwrap()
            .show_equipment
    );
}
