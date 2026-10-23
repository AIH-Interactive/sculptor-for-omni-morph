use super::*;

#[tokio::test]
async fn outfit_recycle_preserves_json_restores_and_purges_content() {
    let server = Server::start().await;
    let configuration =
        serde_json::to_value(crate::service::outfit::OutfitConfig::default()).unwrap();
    let published = server.request(Method::POST, "/api/library/v1/uploads/outfit", 1)
        .header("x-omnimorph-origin", &server.base)
        .json(&json!({"operation_id": library::random_id(), "metadata": {
            "resource_kind": "OUTFIT", "display_name": "Recycle preset", "internal_name": "Preset",
            "authors": [], "category_id": "general", "description": "", "tags": [],
            "visibility": "PUBLIC", "requires_authorization": false}, "configuration": configuration}))
        .send().await.unwrap();
    assert_eq!(published.status(), 200);
    let published: Value = published.json().await.unwrap();
    let id = published["model_id"].as_str().unwrap();
    let path = format!("/api/library/v1/models/{id}");
    let transfer: Value = server.request(Method::POST, &format!("{path}/transfers"), 1)
        .json(&json!({"operation_id": library::random_id(), "purpose": "download", "model_revision": 1}))
        .send().await.unwrap().json().await.unwrap();
    let relative = transfer["relative_path"].as_str().unwrap();
    let download = server
        .request(Method::GET, relative, 1)
        .send()
        .await
        .unwrap();
    assert_eq!(download.status(), 200);
    let hash = download.headers()["x-omnimorph-sha256"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(!download.bytes().await.unwrap().is_empty());
    assert_eq!(
        server
            .request(Method::POST, &format!("{relative}/complete"), 1)
            .json(&json!({"operation_id": library::random_id(), "raw_sha256": hash}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let deleted = server
        .request(Method::DELETE, &path, 1)
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 200);
    let credentials = admin::issue(&server).await;
    let token = credentials["access_token"].as_str().unwrap();
    let request = |method, path: &str| {
        server
            .request(method, path, 4)
            .header("x-omnimorph-origin", &server.base)
            .header("x-omnimorph-admin", token)
    };
    let list = request(Method::GET, "/api/library/v1/admin/recycle")
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200, "{}", list.text().await.unwrap());
    let page: Value = list.json().await.unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["resource_kind"], "OUTFIT");
    assert!(page["items"][0]["model_type"].is_null());
    let preview = format!("/api/library/v1/admin/wardrobe-preview/LIBRARY_RECYCLE/{id}");
    assert_eq!(
        server
            .request(Method::GET, &preview, 1)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let head = request(Method::HEAD, &preview).send().await.unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-type"], "application/json");
    let length = head.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let hash = head.headers()["x-omnimorph-sha256"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(head.bytes().await.unwrap().is_empty());
    let response = request(Method::GET, &preview).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.bytes().await.unwrap();
    assert_eq!(bytes.len(), length);
    assert_eq!(
        faster_hex::hex_string(&crate::storage::sha256(&bytes)),
        hash
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        configuration
    );
    let restore = json!({"operation_id": library::random_id(), "expected_revision": 3});
    for _ in 0..2 {
        assert_eq!(
            request(
                Method::POST,
                &format!("/api/library/v1/admin/models/{id}/restore")
            )
            .json(&restore)
            .send()
            .await
            .unwrap()
            .status(),
            200
        );
    }
    assert_eq!(
        server
            .request(Method::GET, &path, 2)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let restored: Value = server
        .request(Method::GET, &format!("{path}/outfit?revision=1"), 2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(restored["configuration"], configuration);
    assert_eq!(
        request(Method::GET, &preview)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .request(Method::DELETE, &path, 1)
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 4}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let purge = json!({"operation_id": library::random_id(), "expected_revision": 5});
    for _ in 0..2 {
        assert_eq!(
            request(
                Method::POST,
                &format!("/api/library/v1/admin/models/{id}/purge")
            )
            .json(&purge)
            .send()
            .await
            .unwrap()
            .status(),
            200
        );
    }
    assert_eq!(
        request(Method::GET, &preview)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let page: Value = request(Method::GET, "/api/library/v1/admin/recycle")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page["total"], 0);
    server.state.storage.library_read(|database| {
        let body: String = database.query_row("SELECT configuration_json FROM library_outfit_contents WHERE model_id = ?1",
            [id], |row| row.get(0)).unwrap();
        assert!(body.is_empty(), "purge erases the configuration body while retaining revision identity");
        let receipts: i64 = database.query_row("SELECT count(*) FROM library_download_receipts WHERE model_id = ?1 AND completed = 1",
            [id], |row| row.get(0)).unwrap();
        assert_eq!(receipts, 1, "purge preserves completed receipt history and its referenced revision");
        Ok(())
    }).unwrap();
}

#[tokio::test]
async fn published_outfit_uses_json_catalog_policy_and_membership() {
    let server = Server::start().await;
    let (base, bytes) = server.upload_model("PRIVATE").await;
    let mut configuration =
        serde_json::to_value(crate::service::outfit::OutfitConfig::default()).unwrap();
    configuration["base"] = json!({"source": "CLOUD", "origin": server.base,
        "resource_id": base, "revision": 1, "format": "FIGURA",
        "content_hash": faster_hex::hex_string(&crate::storage::sha256(&bytes))});
    let input = json!({"operation_id": library::random_id(), "metadata": {
        "resource_kind": "OUTFIT", "model_type": null, "display_name": "配装预设",
        "internal_name": "Published preset", "authors": ["Creator"], "category_id": "general",
        "description": "Fixed dependencies", "tags": ["preset"], "visibility": "PUBLIC",
        "requires_authorization": false}, "configuration": configuration});
    let mut published = Value::Null;
    for _ in 0..2 {
        let response = server
            .request(Method::POST, "/api/library/v1/uploads/outfit", 1)
            .header("x-omnimorph-origin", &server.base)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        let value: Value = response.json().await.unwrap();
        if !published.is_null() {
            assert_eq!(value, published);
        }
        published = value;
    }
    assert_eq!(published["policy_revision"], 2);
    let id = published["model_id"].as_str().unwrap();
    server
        .state
        .storage
        .library_read(|database| {
            let rows: (bool, i64, i64, i64) = database
                .query_row(
                    "SELECT model_type IS NULL,
                (SELECT count(*) FROM library_revisions r WHERE r.model_id = m.model_id),
                (SELECT count(*) FROM library_outfit_contents c WHERE c.model_id = m.model_id),
                (SELECT count(*) FROM library_collection c WHERE c.model_id = m.model_id)
             FROM library_models m WHERE model_id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(rows, (true, 0, 1, 1));
            Ok(())
        })
        .unwrap();
    let response = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 2)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let detail: Value = response.json().await.unwrap();
    assert_eq!(detail["resource_kind"], "OUTFIT");
    assert!(detail["model_type"].is_null());
    assert!(!detail["allowed_actions"]
        .as_array()
        .unwrap()
        .contains(&json!("equip")));
    let response = server
        .request(
            Method::GET,
            &format!("/api/library/v1/models/{id}/outfit?revision=1"),
            2,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap()["configuration"],
        configuration
    );
    // Public preset references do not change the private dependency's policy or grants.
    assert!(!server
        .request(Method::GET, &format!("/api/library/v1/models/{base}"), 2)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert_eq!(
        server
            .request(
                Method::GET,
                &format!("/api/library/v1/models/{id}/outfit?revision=2"),
                1
            )
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let mut changed = input.clone();
    changed["metadata"]["display_name"] = json!("Changed retry");
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/uploads/outfit", 1)
            .header("x-omnimorph-origin", &server.base)
            .json(&changed)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
}

#[tokio::test]
async fn published_outfit_rejects_missing_local_foreign_and_unauthorized_dependencies() {
    let server = Server::start().await;
    let (base, bytes) = server.upload_model("PRIVATE").await;
    let mut configuration =
        serde_json::to_value(crate::service::outfit::OutfitConfig::default()).unwrap();
    configuration["base"] = json!({"source": "CLOUD", "origin": server.base,
        "resource_id": base, "revision": 1, "format": "FIGURA",
        "content_hash": faster_hex::hex_string(&crate::storage::sha256(&bytes))});
    let mut input = json!({"operation_id": library::random_id(), "metadata": {
        "resource_kind": "OUTFIT", "display_name": "Rejected preset", "internal_name": "Preset",
        "authors": [], "category_id": "general", "description": "", "visibility": "PRIVATE",
        "requires_authorization": true}, "configuration": configuration});
    assert!(!server
        .request(Method::POST, "/api/library/v1/uploads/outfit", 2)
        .header("x-omnimorph-origin", &server.base)
        .json(&input)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    for reference in [
        json!({"source": "LOCAL", "local_id": library::random_id(), "format": "FIGURA", "content_hash": "0".repeat(64)}),
        json!({"source": "CLOUD", "origin": "https://foreign.invalid", "resource_id": base,
            "revision": 1, "format": "FIGURA", "content_hash": "0".repeat(64)}),
        json!({"source": "CLOUD", "origin": server.base, "resource_id": library::random_id(),
            "revision": 1, "format": "FIGURA", "content_hash": "0".repeat(64)}),
        json!({"source": "CLOUD", "origin": server.base, "resource_id": base,
            "revision": 1, "format": "FIGURA", "content_hash": "0".repeat(64)}),
    ] {
        input["operation_id"] = json!(library::random_id());
        input["configuration"]["base"] = reference;
        assert!(!server
            .request(Method::POST, "/api/library/v1/uploads/outfit", 1)
            .header("x-omnimorph-origin", &server.base)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
    }
    server
        .state
        .storage
        .library_read(|database| {
            let count: i64 = database
                .query_row("SELECT count(*) FROM library_outfit_contents", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
            Ok(())
        })
        .unwrap();
}

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
    assert_eq!(capabilities["outfits_v1"]["publication"], true);
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
        config.outfit.limits.max_custom_particles = 7;
        config.outfit.limits.max_particle_layers = 2;
        config.outfit.limits.max_ribbon_vertices = 64;
        config.outfit.limits.max_particle_callbacks_per_tick = 0;
        config.outfit.limits.max_particle_callbacks_per_frame = 1;
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
        capabilities["outfits_v1"]["limits"]["max_custom_particles"],
        7
    );
    assert_eq!(
        capabilities["outfits_v1"]["limits"]["max_particle_layers"],
        2
    );
    assert_eq!(
        capabilities["outfits_v1"]["limits"]["max_ribbon_vertices"],
        64
    );
    assert_eq!(
        capabilities["outfits_v1"]["limits"]["max_particle_callbacks_per_tick"],
        0
    );
    assert_eq!(
        capabilities["outfits_v1"]["limits"]["max_particle_callbacks_per_frame"],
        1
    );
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
