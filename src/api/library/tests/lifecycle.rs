use super::*;

#[tokio::test]
async fn upload_requires_explicit_valid_visibility() {
    let server = Server::start().await;
    for visibility in [None, Some(json!("UNLISTED")), Some(Value::Null)] {
        let mut input = upload_input(&figura_package(), "PUBLIC");
        if let Some(value) = visibility {
            input["metadata"]["visibility"] = value;
        } else {
            input["metadata"]
                .as_object_mut()
                .unwrap()
                .remove("visibility");
        }
        let response = server
            .request(Method::POST, "/api/library/v1/uploads", 1)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert_eq!(
            response.json::<Value>().await.unwrap()["code"],
            "INVALID_VISIBILITY"
        );
    }
}

#[tokio::test]
async fn upload_responses_report_persisted_publication_policy() {
    let server = Server::start().await;
    for visibility in ["PUBLIC", "PRIVATE"] {
        let bytes = figura_package();
        let input = upload_input(&bytes, visibility);
        let response = server
            .request(Method::POST, "/api/library/v1/uploads", 1)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let reservation = response.json::<Value>().await.unwrap();
        assert_eq!(reservation["visibility"], visibility);
        assert_eq!(reservation["published"], false);
        assert_eq!(reservation["moderation_status"], "ACTIVE");
        let response = server
            .request(
                Method::PUT,
                reservation["relative_path"].as_str().unwrap(),
                1,
            )
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 204);
        let response = server
            .request(
                Method::POST,
                &format!(
                    "/api/library/v1/uploads/{}/commit",
                    input["operation_id"].as_str().unwrap()
                ),
                1,
            )
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let committed = response.json::<Value>().await.unwrap();
        assert_eq!(committed["visibility"], visibility);
        assert_eq!(committed["published"], true);
        assert_eq!(committed["moderation_status"], "ACTIVE");
    }
}

#[tokio::test]
async fn upload_commit_reports_moderation_override_instead_of_draft_visibility() {
    let server = Server::start().await;
    let (original, bytes) = server.upload_model("PUBLIC").await;
    let credentials = admin::issue(&server).await;
    let response = server
        .request(
            Method::PUT,
            &format!("/api/library/v1/admin/models/{original}/block"),
            4,
        )
        .header("x-omnimorph-origin", &server.base)
        .header(
            "x-omnimorph-admin",
            credentials["access_token"].as_str().unwrap(),
        )
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let input = upload_input(&bytes, "PUBLIC");
    let reservation = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(reservation["visibility"], "PUBLIC");
    let response = server
        .request(
            Method::PUT,
            reservation["relative_path"].as_str().unwrap(),
            1,
        )
        .header("content-type", "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let response = server
        .request(
            Method::POST,
            &format!(
                "/api/library/v1/uploads/{}/commit",
                input["operation_id"].as_str().unwrap()
            ),
            1,
        )
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let committed = response.json::<Value>().await.unwrap();
    assert_eq!(committed["visibility"], "PRIVATE");
    assert_eq!(committed["moderation_status"], "BLOCKED");
    assert_eq!(committed["published"], true);
    assert_eq!(committed["distribution_code"], "MODERATION_PRIVATE");
}

pub(super) async fn restart(server: &mut Server) {
    server.task.abort();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !server.task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let storage = Arc::new(Storage::open(server._directory.path()).unwrap());
    let manager = Arc::new(crate::auth::UManager::open(storage.clone()).unwrap());
    for player in 1..=4 {
        let user = manager
            .get_by_uuid(&Uuid::from_u128(player))
            .unwrap()
            .clone();
        assert!(user.token.is_none());
        manager
            .insert(user.uuid, format!("synthetic-token-{player}"), user)
            .unwrap();
    }
    let state = AppState {
        uptime: tokio::time::Instant::now(),
        user_manager: manager,
        session: Arc::new(crate::service::session::SessionRegistry::default()),
        subscribes: Arc::new(crate::service::event::EventHub::default()),
        protocol_sessions: Arc::new(crate::service::session::ProtocolSessions::new(
            storage.instance_id(),
        )),
        storage,
        transfers: Arc::new(crate::service::resource::Transfers::default()),
        config: server.state.config.clone(),
        figura_versions: Arc::new(RwLock::new(None)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = router(state.clone())
        .merge(crate::api::protobuf::router(true))
        .route("/ws", axum::routing::get(crate::api::figura::ws))
        .route(
            "/api/:uuid",
            axum::routing::get(crate::api::figura::profile::user_info),
        )
        .route(
            "/api/:uuid/avatar",
            axum::routing::get(crate::api::figura::profile::download_avatar),
        )
        .route(
            "/api/avatar",
            axum::routing::put(crate::api::figura::profile::upload_avatar)
                .layer(axum::extract::DefaultBodyLimit::max(100 * 1024)),
        )
        .with_state(state.clone());
    server.base = format!("http://{}", listener.local_addr().unwrap());
    server.task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    server.state = state;
}

#[tokio::test]
async fn restart_loses_upload_grant_but_reissues_same_draft_without_duplicate_models() {
    let mut server = Server::start().await;
    let bytes = figura_package();
    let input = upload_input(&bytes, "PUBLIC");
    let reservation = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let path = reservation["relative_path"].as_str().unwrap();
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
    restart(&mut server).await;
    assert_eq!(
        server
            .request(Method::PUT, path, 1)
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let repeated = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(reservation["model_id"], repeated["model_id"]);
    assert_eq!(
        server
            .request(Method::PUT, path, 1)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let commit_path = format!(
        "/api/library/v1/uploads/{}/commit",
        input["operation_id"].as_str().unwrap()
    );
    assert_eq!(
        server
            .request(Method::POST, &commit_path, 1)
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    restart(&mut server).await;
    let list = server
        .request(Method::GET, "/api/library/v1/models?scope=mine", 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["model_id"], reservation["model_id"]);
}

#[tokio::test]
async fn concurrent_commit_has_one_publication_and_retry_returns_the_original_result() {
    let server = Server::start().await;
    let bytes = figura_package();
    let input = upload_input(&bytes, "PUBLIC");
    let reservation = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    server
        .request(
            Method::PUT,
            reservation["relative_path"].as_str().unwrap(),
            1,
        )
        .header("content-type", "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .unwrap();
    let path = format!(
        "/api/library/v1/uploads/{}/commit",
        input["operation_id"].as_str().unwrap()
    );
    let commit = json!({"operation_id": library::random_id(), "expected_revision": 1});
    let first = server.request(Method::POST, &path, 1).json(&commit).send();
    let second = server.request(Method::POST, &path, 1).json(&commit).send();
    let (first, second) = tokio::join!(first, second);
    let statuses = [
        first.unwrap().status().as_u16(),
        second.unwrap().status().as_u16(),
    ];
    assert!(statuses.contains(&200));
    assert!(
        statuses.iter().all(|status| matches!(status, 200 | 409)),
        "{statuses:?}"
    );
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&commit)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let detail = server
        .request(
            Method::GET,
            &format!(
                "/api/library/v1/models/{}",
                reservation["model_id"].as_str().unwrap()
            ),
            1,
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(detail["policy_revision"], 2);
    assert_eq!(detail["resource_revision"], 1);
}

#[tokio::test]
async fn wrong_digest_and_damaged_package_fail_without_publication_and_can_be_cancelled() {
    let server = Server::start().await;
    let bytes = figura_package();
    let input = upload_input(&bytes, "PUBLIC");
    let reservation = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let path = reservation["relative_path"].as_str().unwrap();
    assert_eq!(
        server
            .request(Method::PUT, path, 1)
            .header("content-type", "application/octet-stream")
            .body(vec![0; bytes.len()])
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        server
            .request(Method::PUT, path, 1)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let cancel = format!(
        "/api/library/v1/uploads/{}",
        input["operation_id"].as_str().unwrap()
    );
    assert_eq!(
        server
            .request(Method::DELETE, &cancel, 1)
            .json(&json!({"operation_id": library::random_id()}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let damaged = b"not gzip or compressed NBT".to_vec();
    let input = upload_input(&damaged, "PUBLIC");
    let reserved = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        server
            .request(Method::PUT, reserved["relative_path"].as_str().unwrap(), 1)
            .header("content-type", "application/octet-stream")
            .body(damaged)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let commit = format!(
        "/api/library/v1/uploads/{}/commit",
        input["operation_id"].as_str().unwrap()
    );
    assert_eq!(
        server
            .request(Method::POST, &commit, 1)
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let list = server
        .request(Method::GET, "/api/library/v1/models?scope=mine", 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(list["total"], 0);
}

#[tokio::test]
async fn delete_replays_its_own_tombstone_but_does_not_reveal_it_to_other_players() {
    let server = Server::start().await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let path = format!("/api/library/v1/models/{id}");
    let input = json!({"operation_id": library::random_id(), "expected_revision": 2});
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::DELETE, &path, 1)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(
        server
            .request(Method::DELETE, &path, 2)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .request(Method::GET, &path, 1)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn uploader_gets_an_automatic_nonrevocable_grant_when_authorization_is_enabled() {
    let server = Server::start().await;
    let bytes = figura_package();
    let mut input = upload_input(&bytes, "PRIVATE");
    input["metadata"]["requires_authorization"] = json!(true);
    let reservation = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    server
        .request(
            Method::PUT,
            reservation["relative_path"].as_str().unwrap(),
            1,
        )
        .header("content-type", "application/octet-stream")
        .body(bytes)
        .send()
        .await
        .unwrap();
    let path = format!(
        "/api/library/v1/uploads/{}/commit",
        input["operation_id"].as_str().unwrap()
    );
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let id = reservation["model_id"].as_str().unwrap();
    let path = format!("/api/library/v1/models/{id}/grants");
    let grants = server
        .request(Method::GET, &path, 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(grants["total"], 1);
    assert_eq!(grants["items"][0]["uuid"], Uuid::from_u128(1).to_string());
    let path = format!("{path}/{}", Uuid::from_u128(1));
    assert_eq!(
        server
            .request(Method::DELETE, &path, 1)
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 2}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let detail = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(
        detail["allowed_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("download"))
    );
}

#[tokio::test]
async fn errors_have_stable_json_and_request_ids_and_player_revocation_is_rechecked() {
    let server = Server::start().await;
    let response = server
        .request(Method::GET, "/api/library/v1/models?page=not-a-number", 1)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let error = response.json::<Value>().await.unwrap();
    assert_eq!(error["request_id"], request_id);
    assert_eq!(error["code"], "INVALID_REQUEST");
    let user = server
        .state
        .user_manager
        .get_by_uuid(&Uuid::from_u128(1))
        .unwrap()
        .clone();
    server.state.user_manager.ban(&user).unwrap();
    let response = server
        .request(Method::GET, "/api/library/v1/session", 1)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn disconnected_upload_releases_its_writer_and_allows_the_same_grant_to_retry() {
    let server = Server::start().await;
    let bytes = figura_package();
    let input = upload_input(&bytes, "PUBLIC");
    let reservation = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let path = reservation["relative_path"].as_str().unwrap();
    use tokio::io::AsyncWriteExt;

    let address = server.base.strip_prefix("http://").unwrap();
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let headers = format!(
        "PUT {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer synthetic-token-1\r\nx-omnimorph-auth-mode: figura\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n8\r\n"
    );
    socket.write_all(headers.as_bytes()).await.unwrap();
    socket.write_all(&bytes[..8]).await.unwrap();
    socket.write_all(b"\r\n").await.unwrap();
    // Disconnect without the terminating chunk to exercise a broken HTTP body.
    socket.shutdown().await.unwrap();
    drop(socket);
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let response = server
                .request(Method::PUT, path, 1)
                .header("content-type", "application/octet-stream")
                .body(bytes.clone())
                .send()
                .await
                .unwrap();
            if response.status() != 409 {
                break response.status();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result, 204);
}

#[tokio::test]
async fn encrypted_distribution_keeps_ciphertext_identity_and_marks_metadata_client_extracted() {
    let server = Server::start().await;
    // Synthetic parser fixture only; it does not claim that the fake ciphertext renders.
    let (bytes, ciphertext_hash) = crate::service::package::tests::fixture(true, false);
    let mut input = upload_input(&bytes, "PUBLIC");
    input["metadata"]["model_type"] = json!("YSM_ENCRYPTED");
    input["content_sha256"] = json!(faster_hex::hex_string(&ciphertext_hash));
    input["schema_sha256"] = json!("c".repeat(64));
    input["source_filename"] = json!("synthetic.ysm");
    input["metadata_source"] = json!("encrypted_manifest");
    let reservation = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
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
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let id = reservation["model_id"].as_str().unwrap();
    let detail = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(detail["metadata_validation"], "CLIENT_EXTRACTED");
    let hash: String = server.state.storage.library_read(|database| database.query_row(
        "SELECT verified_hash FROM library_revision_hashes WHERE model_id = ?1 AND hash_kind = 'YSM_CIPHERTEXT_SHA256'", [id], |row| row.get(0)).map_err(library::sql_error)).unwrap();
    assert_eq!(hash, faster_hex::hex_string(&ciphertext_hash));
}

#[tokio::test]
#[ignore = "Run in a separate bounded worker-saturation batch; intentionally fills the process-wide worker pool"]
async fn worker_saturation_returns_503_and_recovers_after_permits_are_released() {
    use std::sync::{Condvar, Mutex};
    let server = Server::start().await;
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, mut arrivals) = tokio::sync::mpsc::channel(8);
    let mut workers = Vec::new();
    for _ in 0..8 {
        let gate = gate.clone();
        let entered = entered.clone();
        workers.push(tokio::spawn(library::work(move || {
            entered.blocking_send(()).map_err(|_| ErrorCode::Internal)?;
            let released = gate.0.lock().map_err(|_| ErrorCode::Internal)?;
            let (released, timeout) = gate
                .1
                .wait_timeout_while(released, std::time::Duration::from_secs(5), |released| {
                    !*released
                })
                .map_err(|_| ErrorCode::Internal)?;
            if timeout.timed_out() && !*released {
                return Err(ErrorCode::Offline);
            }
            Ok(())
        })));
    }
    for _ in 0..8 {
        tokio::time::timeout(std::time::Duration::from_secs(2), arrivals.recv())
            .await
            .unwrap()
            .unwrap();
    }
    let response = server
        .request(Method::GET, "/api/library/v1/models", 1)
        .send()
        .await
        .unwrap();
    let status = response.status();
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    for worker in workers {
        worker.await.unwrap().unwrap();
    }
    assert_eq!(status, 503);
    assert_eq!(
        server
            .request(Method::GET, "/api/library/v1/models", 1)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}
