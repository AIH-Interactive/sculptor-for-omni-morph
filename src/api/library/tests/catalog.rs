use super::*;

#[tokio::test]
async fn name_sort_is_server_paginated_and_uses_model_id_for_case_insensitive_ties() {
    let server = Server::start().await;
    let mut expected = Vec::new();
    for name in ["beta", "Alpha", "alpha"] {
        let (id, _) = server.upload_model("PUBLIC").await;
        let path = format!("/api/library/v1/models/{id}");
        let detail = server
            .request(Method::GET, &path, 1)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        let changed = server
            .request(Method::PATCH, &path, 1)
            .json(&json!({
                "operation_id": library::random_id(),
                "expected_revision": detail["policy_revision"],
                "display_name": name,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(changed.status(), 200);
        expected.push((name.to_ascii_lowercase(), id));
    }
    expected.sort();
    let mut actual = Vec::new();
    for page in 1..=3 {
        let response = server
            .request(
                Method::GET,
                &format!("/api/library/v1/models?sort=name&page_size=1&page={page}"),
                2,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body = response.json::<Value>().await.unwrap();
        assert_eq!(body["total"], 3);
        assert_eq!(body["page_size"], 1);
        assert_eq!(body["has_next"], page < 3);
        actual.push(body["items"][0]["model_id"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        actual,
        expected.into_iter().map(|(_, id)| id).collect::<Vec<_>>()
    );
    assert_eq!(
        server
            .request(Method::GET, "/api/library/v1/models?sort=arbitrary_sql", 2)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn resource_kind_filter_is_applied_before_catalog_count_and_pagination() {
    let server = Server::start().await;
    let (avatar, _) = server.upload_model("PUBLIC").await;
    let (accessory, _) = server.upload_model("PUBLIC").await;
    let (other, _) = server.upload_model("PUBLIC").await;
    // Seed classification only: this test exercises listing, not accessory package validation.
    server
        .state
        .storage
        .library_read(|database| {
            database
                .execute(
                    "INSERT OR REPLACE INTO library_resource_kinds VALUES (?1, 'ACCESSORY')",
                    [&accessory],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    for scope in ["public", "mine"] {
        for (kind, total) in [("AVATAR", 2), ("ACCESSORY", 1)] {
            let response = server
                .request(
                    Method::GET,
                    &format!(
                        "/api/library/v1/models?scope={scope}&resource_kind={kind}&page_size=1"
                    ),
                    1,
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let body = response.json::<Value>().await.unwrap();
            assert_eq!(body["total"], total);
            assert_eq!(body["items"].as_array().unwrap().len(), 1);
            assert_eq!(body["items"][0]["resource_kind"], kind);
            if kind == "ACCESSORY" {
                assert_eq!(body["items"][0]["model_id"], accessory);
                assert_eq!(body["has_next"], false);
            } else {
                let id = body["items"][0]["model_id"].as_str().unwrap();
                assert!(id == avatar || id == other);
                assert_eq!(body["has_next"], true);
            }
        }
    }
    let invalid = server
        .request(
            Method::GET,
            "/api/library/v1/models?resource_kind=INVALID",
            1,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
}
