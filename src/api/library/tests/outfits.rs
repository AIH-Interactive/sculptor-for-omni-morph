use super::*;

#[tokio::test]
async fn owner_presets_round_trip_with_cas_and_authentication() {
    let server = Server::start().await;
    let capabilities: Value = server
        .request(Method::GET, "/api/library/v1/capabilities", 1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(capabilities["outfits_v1"]["presets"], true);
    assert_eq!(capabilities["outfits_v1"]["apply"], false);
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/outfit-v1/duplicate-instances.json"
    ))
    .unwrap();
    let id = library::random_id();
    let mut input = json!({"operation_id": library::random_id(), "preset_id": id,
        "expected_revision": 0, "name": "星翼 #1/#2", "configuration": fixture});
    let path = "/api/library/v1/outfit-presets";
    for _ in 0..2 {
        let response = server
            .request(Method::POST, path, 1)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let saved: Value = response.json().await.unwrap();
        assert_eq!(saved["configuration"], fixture);
        assert_eq!(saved["revision"], 1);
    }
    input["name"] = json!("conflicting retry");
    assert_eq!(
        server
            .request(Method::POST, path, 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let path = format!("{path}/{id}");
    assert_eq!(
        server
            .request(Method::GET, &path, 2)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .client
            .get(format!("{}{path}", server.base))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        server
            .request(Method::GET, &path, 1)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn accessory_purpose_survives_upload_and_is_not_legacy_base_equipment() {
    let server = Server::start().await;
    let bytes = figura_package();
    let mut input = upload_input(&bytes, "PUBLIC");
    input["metadata_source"] = json!("cosmetic.json");
    input["metadata"]["resource_kind"] = json!("ACCESSORY");
    input["metadata"]["accessory_metadata"] = json!({
        "schema_version": 1, "category": "FutureCategory", "cosmetic_anchor": "wing_anchor",
        "default_transform": {"position": [0,0,0], "rotation": [0,0,0], "scale": [1,1,1]},
        "supported_bases": ["VANILLA", "FIGURA", "YSM"], "has_bones": false,
        "skeleton_revision": "", "skeleton_hash": "", "display_bones": false,
        "edit_bones": false, "exported_bones": [], "validation_source": "SERVER_VERIFIED"
    });
    let response = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let reservation: Value = response.json().await.unwrap();
    assert_eq!(
        server
            .request(
                Method::PUT,
                reservation["relative_path"].as_str().unwrap(),
                1
            )
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let path = format!(
        "/api/library/v1/uploads/{}/commit",
        input["operation_id"].as_str().unwrap()
    );
    let response = server
        .request(Method::POST, &path, 1)
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let id = reservation["model_id"].as_str().unwrap();
    let path = format!("/api/library/v1/models/{id}");
    let detail: Value = server
        .request(Method::GET, &path, 1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["resource_kind"], "ACCESSORY");
    assert_eq!(detail["model_type"], "FIGURA");
    assert_eq!(detail["accessory_metadata"]["category"], "FutureCategory");
    assert_eq!(
        detail["accessory_metadata"]["validation_source"],
        "CLIENT_EXTRACTED"
    );
    assert!(!detail["allowed_actions"]
        .as_array()
        .unwrap()
        .contains(&json!("equip")));
    assert_eq!(
        server
            .request(Method::POST, &format!("{path}/equip"), 1)
            .json(
                &json!({"operation_id": library::random_id(), "model_revision": 1,
            "expected_equipment_revision": 0})
            )
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
}

#[tokio::test]
async fn configured_limits_are_advertised_and_enforced_after_config_change() {
    let server = Server::start().await;
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/outfit-v1/duplicate-instances.json"
    ))
    .unwrap();
    let mut input = json!({"operation_id": library::random_id(), "preset_id": library::random_id(),
        "expected_revision": 0, "name": "configured limits", "configuration": fixture});
    {
        let mut config = server.state.config.write().await;
        config.outfit.limits.max_instances = 1;
        config.outfit.limits.max_trails = 0;
        config.outfit.limits.max_presets = 1;
        config.outfit.limits.dynamic_lighting_enabled = false;
    }
    let capabilities: Value = server
        .request(Method::GET, "/api/library/v1/capabilities", 1)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(capabilities["outfits_v1"]["limits"]["max_instances"], 1);
    assert_eq!(capabilities["outfits_v1"]["limits"]["max_trails"], 0);
    assert_eq!(
        capabilities["outfits_v1"]["limits"]["dynamic_lighting_enabled"],
        false
    );
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/outfit-presets", 1)
            .json(&input)
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
        .max_instances = 2;
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/outfit-presets", 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    input["operation_id"] = json!(library::random_id());
    input["preset_id"] = json!(library::random_id());
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/outfit-presets", 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
    let mut config = server.state.config.write().await;
    config.outfit.limits.max_controls = 0;
    drop(config);
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/outfit-presets", 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}
