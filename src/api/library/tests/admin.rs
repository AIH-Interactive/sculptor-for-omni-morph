use super::*;
use std::sync::LazyLock;

static ADMIN_TESTS: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

async fn configure(server: &Server) {
    // Independently derived, synthetic M1 password vector; never a deployment credential.
    let config: crate::service::library_admin::LibraryConfig = toml::from_str(&format!(
        "[[admins]]\nplayer_name = \"Player4\"\nuuid = \"{}\"\npassword_hash = \"pbkdf2-sha256$v1$600000$000102030405060708090a0b0c0d0e0f$055bcd61a2939b3846e10d4f9ebdf02be6610f5f894fd7882e985b9f87b81cff\"",
        Uuid::from_u128(4))).unwrap();
    server.state.config.write().await.model_library.admins = config.admins;
}

fn request(server: &Server, method: Method, path: &str) -> reqwest::RequestBuilder {
    server
        .request(method, path, 4)
        .header("x-omnimorph-origin", &server.base)
}

async fn login(server: &Server, operation: &str) -> Value {
    let response = request(server, Method::POST, "/api/library/v1/admin/login")
        .json(&json!({"operation_id": operation, "player_name": "Player4", "password": "synthetic-test-password", "remember": true})).send().await.unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    assert_eq!(response.headers()["cache-control"], "no-store");
    response.json().await.unwrap()
}

#[tokio::test]
async fn login_refresh_and_logout_are_bound_idempotent_and_refresh_reuse_revokes_family() {
    let _serial = ADMIN_TESTS.lock().await;
    let server = Server::start().await;
    configure(&server).await;
    let operation = library::random_id();
    let first = login(&server, &operation).await;
    let second = login(&server, &operation).await;
    assert_eq!(first, second);
    let input =
        json!({"operation_id": library::random_id(), "refresh_token": first["refresh_token"]});
    let response = request(&server, Method::POST, "/api/library/v1/admin/refresh")
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let refreshed = response.json::<Value>().await.unwrap();
    let repeated = request(&server, Method::POST, "/api/library/v1/admin/refresh")
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(repeated.status(), 200);
    assert_eq!(repeated.json::<Value>().await.unwrap(), refreshed);
    let response = request(&server, Method::GET, "/api/library/v1/session")
        .header(
            "x-omnimorph-admin",
            refreshed["access_token"].as_str().unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["admin"], true);
    let replay =
        json!({"operation_id": library::random_id(), "refresh_token": first["refresh_token"]});
    assert_eq!(
        request(&server, Method::POST, "/api/library/v1/admin/refresh")
            .json(&replay)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        request(&server, Method::GET, "/api/library/v1/session")
            .header(
                "x-omnimorph-admin",
                refreshed["access_token"].as_str().unwrap()
            )
            .send()
            .await
            .unwrap()
            .status(),
        401
    );

    let credentials = login(&server, &library::random_id()).await;
    let logout = json!({"operation_id": library::random_id()});
    for _ in 0..2 {
        assert_eq!(
            request(&server, Method::POST, "/api/library/v1/admin/logout")
                .header(
                    "x-omnimorph-admin",
                    credentials["access_token"].as_str().unwrap()
                )
                .json(&logout)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(
        request(&server, Method::GET, "/api/library/v1/session")
            .header(
                "x-omnimorph-admin",
                credentials["access_token"].as_str().unwrap()
            )
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn administrator_uuid_origin_password_config_and_login_rate_are_enforced() {
    let _serial = ADMIN_TESTS.lock().await;
    let server = Server::start().await;
    configure(&server).await;
    let input = json!({"operation_id": library::random_id(), "player_name": "Player4", "password": "synthetic-test-password", "remember": false});
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/admin/login", 3)
            .header("x-omnimorph-origin", &server.base)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        request(&server, Method::POST, "/api/library/v1/admin/login")
            .header("x-omnimorph-origin", "https://other.invalid")
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let credentials = login(&server, &library::random_id()).await;
    let mut changed = server.state.config.read().await.model_library.clone();
    changed.admins.clear();
    {
        let mut config = server.state.config.write().await;
        server
            .state
            .storage
            .library_admin_reconcile(&changed)
            .unwrap();
        config.model_library = changed;
    }
    assert_eq!(
        request(&server, Method::GET, "/api/library/v1/session")
            .header(
                "x-omnimorph-admin",
                credentials["access_token"].as_str().unwrap()
            )
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    configure(&server).await;
    assert_eq!(
        request(&server, Method::GET, "/api/library/v1/session")
            .header(
                "x-omnimorph-admin",
                credentials["access_token"].as_str().unwrap()
            )
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    for _ in 0..4 {
        let wrong = json!({"operation_id": library::random_id(), "player_name": "Player4", "password": "wrong", "remember": false});
        assert_eq!(
            request(&server, Method::POST, "/api/library/v1/admin/login")
                .json(&wrong)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    let limited = request(&server, Method::POST, "/api/library/v1/admin/login")
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status(), 429);
}

#[tokio::test]
async fn block_hides_from_uploader_and_unblock_keeps_private_with_persistent_hashes() {
    let _serial = ADMIN_TESTS.lock().await;
    let server = Server::start().await;
    configure(&server).await;
    let (model, bytes) = server.upload_model("PUBLIC").await;
    let credentials = login(&server, &library::random_id()).await;
    let token = credentials["access_token"].as_str().unwrap();
    let path = format!("/api/library/v1/admin/models/{model}/block");
    let input = json!({"operation_id": library::random_id(), "expected_revision": 2, "reason": "synthetic moderation"});
    assert_eq!(
        server
            .request(Method::PUT, &path, 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    for _ in 0..2 {
        assert_eq!(
            request(&server, Method::PUT, &path)
                .header("x-omnimorph-admin", token)
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
            .request(Method::GET, &format!("/api/library/v1/models/{model}"), 1)
            .send()
            .await
            .unwrap()
            .status(),
        404
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
    assert!(
        server
            .state
            .storage
            .library_hash_blocked(
                "RAW_SHA256",
                &faster_hex::hex_string(&crate::storage::sha256(&bytes))
            )
            .unwrap()
    );
    let unblock = json!({"operation_id": library::random_id(), "expected_revision": 3});
    assert_eq!(
        request(&server, Method::DELETE, &path)
            .header("x-omnimorph-admin", token)
            .json(&unblock)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let detail = server
        .request(Method::GET, &format!("/api/library/v1/models/{model}"), 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(detail["visibility"], "PRIVATE");
    assert_eq!(detail["moderation_status"], "ACTIVE");
}
