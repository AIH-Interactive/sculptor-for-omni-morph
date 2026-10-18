use super::*;

#[tokio::test]
async fn reports_are_private_idempotent_and_do_not_change_model_policy() {
    let server = Server::start().await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let baseline = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 3)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let path = format!("/api/library/v1/models/{id}/reports");
    let input = json!({"operation_id": library::random_id(), "reason": "Synthetic report reason"});
    let mut responses = Vec::new();
    for _ in 0..2 {
        let response = server.request(Method::POST, &path, 2).json(&input).send().await.unwrap();
        assert_eq!(response.status(), 200);
        responses.push(response.json::<Value>().await.unwrap());
    }
    assert_eq!(responses[0], responses[1]);
    let duplicate = json!({"operation_id": library::random_id(), "reason": "Another reason"});
    assert_eq!(
        server.request(Method::POST, &path, 2).json(&duplicate).send().await.unwrap().status(),
        409
    );
    let forged = json!({"operation_id": library::random_id(), "reason": "Forged", "reporter_uuid": Uuid::from_u128(1)});
    assert_eq!(
        server.request(Method::POST, &path, 3).json(&forged).send().await.unwrap().status(),
        400
    );
    let invalid = json!({"operation_id": library::random_id(), "reason": "x".repeat(513)});
    assert_eq!(
        server.request(Method::POST, &path, 3).json(&invalid).send().await.unwrap().status(),
        400
    );
    let list = "/api/library/v1/admin/reports?page=1&resolved=false";
    assert_eq!(server.request(Method::GET, list, 2).send().await.unwrap().status(), 403);
    assert_eq!(
        server.client.get(format!("{}{list}", server.base)).send().await.unwrap().status(),
        401
    );

    let credentials = admin::issue(&server).await;
    let admin_request = |method, path: &str| {
        server
            .request(method, path, 4)
            .header("x-omnimorph-origin", &server.base)
            .header("x-omnimorph-admin", credentials["access_token"].as_str().unwrap())
    };
    let response = admin_request(Method::GET, list).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let page = response.json::<Value>().await.unwrap();
    assert_eq!(page["total"], 1);
    assert_eq!(page["items"][0]["reporter_uuid"], Uuid::from_u128(2).to_string());
    assert_eq!(page["items"][0]["reason"], input["reason"]);
    let resolve = format!(
        "/api/library/v1/admin/reports/{}/resolve",
        responses[0]["report_id"].as_str().unwrap()
    );
    let mutation = json!({"operation_id": library::random_id(), "expected_revision": 1});
    assert_eq!(
        server.request(Method::POST, &resolve, 2).json(&mutation).send().await.unwrap().status(),
        403
    );
    for _ in 0..2 {
        assert_eq!(
            admin_request(Method::POST, &resolve).json(&mutation).send().await.unwrap().status(),
            200
        );
    }
    let resolved = admin_request(Method::GET, "/api/library/v1/admin/reports?resolved=true")
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(resolved["total"], 1);
    assert_eq!(resolved["items"][0]["resolved_by"], Uuid::from_u128(4).to_string());
    let model = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 3)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(model["visibility"], "PUBLIC");
    assert_eq!(model["policy_revision"], baseline["policy_revision"]);
    assert!(model.get("reports").is_none());

    let (private, _) = server.upload_model("PRIVATE").await;
    let hidden = format!("/api/library/v1/models/{private}/reports");
    assert_eq!(
        server.request(Method::POST, &hidden, 2).json(&input).send().await.unwrap().status(),
        404
    );
    server
        .state
        .storage
        .library_read(|database| {
            let value: String = database
                .query_row("SELECT reason FROM library_reports", [], |row| row.get(0))
                .unwrap();
            assert_eq!(value, "Synthetic report reason");
            Ok(())
        })
        .unwrap();
    // Reopening the same durable store preserves the resolved record.
    let reopened = Storage::open(server._directory.path()).unwrap();
    let count: i64 = reopened
        .library_read(|database| {
            Ok(database
                .query_row(
                    "SELECT count(*) FROM library_reports WHERE resolved_at IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .unwrap())
        })
        .unwrap();
    assert_eq!(count, 1);
}
