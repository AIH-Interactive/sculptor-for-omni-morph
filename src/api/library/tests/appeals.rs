use super::*;

#[tokio::test]
async fn uploader_appeals_are_isolated_and_review_unblock_preserves_private_state() {
    let server = Server::start().await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let credentials = admin::issue(&server).await;
    let request = |method, path: &str| {
        server
            .request(method, path, 4)
            .header("x-omnimorph-origin", &server.base)
            .header("x-omnimorph-admin", credentials["access_token"].as_str().unwrap())
    };
    let block = format!("/api/library/v1/admin/models/{id}/block");
    assert_eq!(
        request(Method::PUT, &block)
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 2}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let target_path = "/api/library/v1/appeal-targets?page=1";
    let targets = server
        .request(Method::GET, target_path, 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(targets["total"], 1);
    assert_eq!(targets["items"][0]["target_id"], id);
    assert!(targets["items"][0].get("image_available").is_none());
    let other = server
        .request(Method::GET, target_path, 2)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(other["total"], 0);
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{id}"), 1)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let input = json!({"operation_id": library::random_id(), "target_kind": "LIBRARY", "target_id": id,
        "reason": "Synthetic uploader appeal"});
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/appeals", 2)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let first = server
        .request(Method::POST, "/api/library/v1/appeals", 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let appeal = first.json::<Value>().await.unwrap()["appeal_id"].as_str().unwrap().to_owned();
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/appeals", 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let mut duplicate = input.clone();
    duplicate["operation_id"] = json!(library::random_id());
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/appeals", 1)
            .json(&duplicate)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let admin_list = "/api/library/v1/admin/appeals?resolved=false";
    assert_eq!(server.request(Method::GET, admin_list, 1).send().await.unwrap().status(), 403);
    let page =
        request(Method::GET, admin_list).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(page["total"], 1);
    let other = server
        .request(Method::GET, "/api/library/v1/appeals", 2)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(other["total"], 0);
    let unblock = format!("/api/library/v1/admin/appeals/{appeal}/unblock");
    let lift = json!({"operation_id": library::random_id(), "expected_revision": 1});
    assert_eq!(
        server.request(Method::POST, &unblock, 1).json(&lift).send().await.unwrap().status(),
        403
    );
    for _ in 0..2 {
        let response = request(Method::POST, &unblock).json(&lift).send().await.unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    }
    let model = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(model["visibility"], "PRIVATE");
    assert_eq!(model["moderation_status"], "ACTIVE");
    let resolve = format!("/api/library/v1/admin/appeals/{appeal}/resolve");
    let resolution = json!({"operation_id": library::random_id(), "expected_revision": 2, "response": "Reviewed; source unblocked, kept private"});
    for _ in 0..2 {
        assert_eq!(
            request(Method::POST, &resolve).json(&resolution).send().await.unwrap().status(),
            200
        );
    }
    // The uploader can replay a lost submission even after the reviewed source is unblocked.
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/appeals", 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let own = server
        .request(Method::GET, "/api/library/v1/appeals?resolved=true", 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(own["items"][0]["response"], resolution["response"]);
    assert!(own["items"][0]["resolved_by"].is_null());
    let reopened = Storage::open(server._directory.path()).unwrap();
    let count: i64 = reopened
        .library_read(|database| {
            Ok(database
                .query_row(
                    "SELECT count(*) FROM library_appeals WHERE resolved_at IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .unwrap())
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn wardrobe_visibility_defaults_public_is_resource_scoped_and_cannot_override_a_ban() {
    let server = Server::start().await;
    let bytes = figura_package();
    wardrobe::upload(&server, 21, &bytes).await;
    let state = server
        .request(Method::GET, "/api/library/v1/wardrobe", 21)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(state["show_equipment"], true);
    assert_eq!(state["blocked"], false);
    let hide = json!({"operation_id": library::random_id(), "expected_equipment_revision": state["equipment_revision"],
        "asset_id": state["asset_id"], "asset_revision": state["asset_revision"], "show_equipment": false});
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::PATCH, "/api/library/v1/wardrobe", 21)
                .json(&hide)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert!(server
        .state
        .storage
        .library_project_equipped(Uuid::from_u128(21), Uuid::from_u128(2))
        .unwrap()
        .primary
        .is_none());
    assert!(server
        .state
        .storage
        .library_project_equipped(Uuid::from_u128(21), Uuid::from_u128(21))
        .unwrap()
        .primary
        .is_some());
    // The same target resource cannot be changed through another authenticated player's session.
    assert_eq!(
        server
            .request(Method::PATCH, "/api/library/v1/wardrobe", 2)
            .json(&hide)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let hidden = server
        .request(Method::GET, "/api/library/v1/wardrobe", 21)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let credentials = admin::issue(&server).await;
    let request = |method, path: &str| {
        server
            .request(method, path, 4)
            .header("x-omnimorph-origin", &server.base)
            .header("x-omnimorph-admin", credentials["access_token"].as_str().unwrap())
    };
    let block_operation = library::random_id();
    let block = format!("/api/library/v1/admin/wardrobe/{}/block", Uuid::from_u128(21));
    assert_eq!(request(Method::POST, &block).json(&json!({"operation_id": block_operation,
        "expected_equipment_revision": hidden["equipment_revision"], "asset_id": hidden["asset_id"], "asset_revision": hidden["asset_revision"]}))
        .send().await.unwrap().status(), 200);
    let reopen = json!({"operation_id": library::random_id(), "expected_equipment_revision": hidden["equipment_revision"],
        "asset_id": hidden["asset_id"], "asset_revision": hidden["asset_revision"], "show_equipment": true});
    assert_eq!(
        server
            .request(Method::PATCH, "/api/library/v1/wardrobe", 21)
            .json(&reopen)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let input = json!({"operation_id": library::random_id(), "target_kind": "WARDROBE", "target_id": block_operation,
        "reason": "Synthetic wardrobe appeal"});
    let response = server
        .request(Method::POST, "/api/library/v1/appeals", 21)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let appeal = response.json::<Value>().await.unwrap()["appeal_id"].as_str().unwrap().to_owned();
    let path = format!("/api/library/v1/admin/appeals/{appeal}/unblock");
    let response = request(Method::POST, &path)
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let after = server
        .request(Method::GET, "/api/library/v1/wardrobe", 21)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(after["blocked"], false);
    assert_eq!(after["show_equipment"], false);
    // Uploading identical immutable bytes keeps the same resource and its existing setting.
    wardrobe::upload(&server, 21, &bytes).await;
    let identical = server
        .request(Method::GET, "/api/library/v1/wardrobe", 21)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(identical["show_equipment"], false);
    // A new immutable upload gets the wardrobe default instead of inheriting the old resource's setting.
    wardrobe::upload(&server, 21, &wardrobe::repack(&bytes, true)).await;
    let fresh = server
        .request(Method::GET, "/api/library/v1/wardrobe", 21)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(fresh["show_equipment"], true);
    assert!(server
        .state
        .storage
        .library_project_equipped(Uuid::from_u128(21), Uuid::from_u128(2))
        .unwrap()
        .primary
        .is_some());
}
