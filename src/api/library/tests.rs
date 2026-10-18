use super::*;
use crate::{
    auth::Userinfo,
    service::{library, library_policy::Visibility},
    storage::Storage,
};
use reqwest::{Client, Method};
use rusqlite::params;
use std::sync::Arc;
use tokio::sync::RwLock;

mod admin;
mod catalog;
mod recycle;
mod reports;
mod appeals;
mod acceptance;
mod equipment;
mod images;
mod lifecycle;
mod transfers;
mod wardrobe;
mod subscriptions;

static TEST_SERVERS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(4)));

static LEGACY_DIRECTORY: std::sync::LazyLock<tempfile::TempDir> =
    std::sync::LazyLock::new(|| {
        let directory = tempfile::tempdir().unwrap();
        std::env::set_var(crate::AVATARS_ENV, directory.path());
        std::env::set_var(crate::ASSETS_ENV, directory.path().join("assets"));
        lazy_static::initialize(&crate::AVATARS_VAR);
        lazy_static::initialize(&crate::ASSETS_VAR);
        directory
    });

struct Server {
    _permit: tokio::sync::OwnedSemaphorePermit,
    _directory: tempfile::TempDir,
    state: AppState,
    base: String,
    client: Client,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn upload_model(&self, visibility: &str) -> (String, Vec<u8>) {
        let bytes = figura_package();
        let input = upload_input(&bytes, visibility);
        let response = self
            .request(Method::POST, "/api/library/v1/uploads", 1)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let reservation = response.json::<Value>().await.unwrap();
        let path = reservation["relative_path"].as_str().unwrap();
        assert_eq!(
            self.request(Method::PUT, path, 1)
                .header("content-type", "application/octet-stream")
                .body(bytes.clone())
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
        let response = self
            .request(Method::POST, &path, 1)
            .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        (reservation["model_id"].as_str().unwrap().to_owned(), bytes)
    }

    async fn start() -> Self {
        Self::start_with_layout(false).await
    }

    async fn start_with_layout(production: bool) -> Self {
        std::sync::LazyLock::force(&LEGACY_DIRECTORY);
        let permit = TEST_SERVERS.clone().acquire_owned().await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let storage = if production {
            Storage::open_avatars(directory.path().join("data/avatars"), directory.path().join("data/metadata.sqlite3"))
        } else {
            Storage::open(directory.path())
        };
        let storage = Arc::new(storage.unwrap());
        let mut config: crate::state::Config =
            toml::from_str(include_str!("../../../Config.example.toml")).unwrap();
        config.limitations.can_upload = true;
        let manager = Arc::new(crate::auth::UManager::open(storage.clone()).unwrap());
        for id in 1..=4 {
            manager
                .insert(
                    Uuid::from_u128(id),
                    format!("synthetic-token-{id}"),
                    Userinfo {
                        uuid: Uuid::from_u128(id),
                        nickname: format!("Player{id}"),
                        ..Default::default()
                    },
                )
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
            config: Arc::new(RwLock::new(config)),
            figura_versions: Arc::new(RwLock::new(None)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = router(state.clone())
            .merge(crate::api::protobuf::router(true))
            .nest("/api/v1", crate::api::v1::router(100 * 1024))
            .route(
                "/internal/:uuid/avatar",
                axum::routing::put(crate::api::lambda::internal::upload_avatar),
            )
            .route(
                "/internal/:uuid/temp",
                axum::routing::put(crate::api::lambda::internal::temp_avatar),
            )
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
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _permit: permit,
            _directory: directory,
            state,
            base,
            client: Client::new(),
            task,
        }
    }

    fn request(&self, method: Method, path: &str, player: u128) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .header("x-omnimorph-auth-mode", "figura")
            .bearer_auth(format!("synthetic-token-{player}"))
    }

    fn seed(
        &self,
        visibility: &str,
        authorization: bool,
        moderation: &str,
        published: bool,
    ) -> String {
        let id = library::random_id();
        self.state
            .storage
            .library_transaction(|transaction| {
                transaction
                    .execute(
                        "INSERT INTO library_models VALUES
                (?1, ?2, 'Player1', '长中文示例', 'Internal', '[\"作者一\",\"Second\"]', 'FIGURA',
                 'general', 'Synthetic description', ?3, ?4, ?5, 1, 1, 1, 1, ?6)",
                        params![
                            id,
                            Uuid::from_u128(1).to_string(),
                            visibility,
                            authorization,
                            moderation,
                            published
                        ],
                    )
                    .map_err(library::sql_error)?;
                transaction
                    .execute(
                        "INSERT INTO library_grants VALUES (?1, ?2, 'Player2', ?3, 1)",
                        params![
                            id,
                            Uuid::from_u128(2).to_string(),
                            Uuid::from_u128(1).to_string()
                        ],
                    )
                    .map_err(library::sql_error)?;
                Ok(())
            })
            .unwrap();
        id
    }
}

fn figura_package() -> Vec<u8> {
    use std::io::Write;
    fn string(output: &mut Vec<u8>, value: &str) {
        output.extend_from_slice(&(value.len() as u16).to_be_bytes());
        output.extend_from_slice(value.as_bytes());
    }
    let mut nbt = vec![10, 0, 0, 10];
    string(&mut nbt, "metadata");
    for (name, value) in [("name", "Internal"), ("authors", "作者一\nSecond")] {
        nbt.push(8);
        string(&mut nbt, name);
        string(&mut nbt, value);
    }
    nbt.extend_from_slice(&[0, 0]);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&nbt).unwrap();
    encoder.finish().unwrap()
}

fn upload_input(bytes: &[u8], visibility: &str) -> Value {
    json!({"operation_id": library::random_id(), "metadata": {
        "display_name": "展示名", "internal_name": "Internal", "authors": ["作者一", "Second"],
        "model_type": "FIGURA", "category_id": "general", "description": "Synthetic upload",
        "tags": ["示例"], "visibility": visibility, "requires_authorization": false,
    }, "raw_sha256": faster_hex::hex_string(&crate::storage::sha256(bytes)), "byte_length": bytes.len(),
        "content_sha256": null, "schema_sha256": null, "source_filename": "synthetic.zip", "metadata_source": "avatar.json"})
}

#[tokio::test]
async fn reserve_stream_and_atomic_commit_preserve_identity_and_old_routes_cannot_publish_pending()
{
    let server = Server::start().await;
    let bytes = figura_package();
    let input = upload_input(&bytes, "PRIVATE");
    let first = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let reservation = first.json::<Value>().await.unwrap();
    let second = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(reservation["model_id"], second["model_id"]);
    let operation = input["operation_id"].as_str().unwrap();
    let body_path = reservation["relative_path"].as_str().unwrap();
    let model_id = reservation["model_id"].as_str().unwrap();
    let asset = format!("library_{}", model_id.replace('-', ""));
    let pending_list = server
        .request(Method::GET, "/api/library/v1/models?scope=mine", 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(pending_list["total"], 0);
    let fake_commit = crate::protocol::v1::CommitUpload {
        operation_id: operation.into(),
        raw_sha256: crate::storage::sha256(&bytes),
        raw_length: bytes.len() as u64,
    };
    assert!(matches!(
        crate::service::resource::commit_upload(&server.state, Uuid::from_u128(1), &fake_commit)
            .await,
        Err(ErrorCode::Forbidden)
    ));
    assert!(
        server
            .state
            .storage
            .avatar(Uuid::from_u128(1), &asset, 1)
            .is_err()
    );
    assert_eq!(
        server
            .request(Method::PUT, body_path, 2)
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        server
            .request(Method::PUT, body_path, 1)
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let commit_input = json!({"operation_id": library::random_id(), "expected_revision": 1});
    let commit_path = format!("/api/library/v1/uploads/{operation}/commit");
    for _ in 0..2 {
        let result = server
            .request(Method::POST, &commit_path, 1)
            .json(&commit_input)
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), 200, "{}", result.text().await.unwrap());
    }
    let detail = server
        .request(
            Method::GET,
            &format!("/api/library/v1/models/{model_id}"),
            1,
        )
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(detail["metadata_validation"], "SERVER_VERIFIED");
    assert_eq!(detail["authors"], json!(["作者一", "Second"]));
    assert_eq!(detail["uploader"]["uuid"], Uuid::from_u128(1).to_string());
    assert_eq!(detail["byte_length"], bytes.len());
    assert_eq!(
        server
            .state
            .storage
            .avatar(Uuid::from_u128(1), &asset, 1)
            .unwrap()
            .raw_sha256,
        crate::storage::sha256(&bytes)
    );
    assert_eq!(
        server
            .request(
                Method::GET,
                &format!("/api/library/v1/models/{model_id}"),
                2
            )
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let scope = server
        .state
        .protocol_sessions
        .issue(
            &server.state,
            &crate::protocol::v1::Auth {
                token: "synthetic-token-2".into(),
                instance_id: server.state.protocol_sessions.instance_id.clone(),
                ..Default::default()
            },
            77,
        )
        .unwrap();
    let legacy_url = format!(
        "{}/api/protobuf/v1/resources/{}/{asset}/1",
        server.base,
        Uuid::from_u128(1)
    );
    assert_eq!(
        server
            .client
            .get(legacy_url)
            .bearer_auth(scope.session_id)
            .header("x-omnimorph-epoch", "77")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn malformed_metadata_commit_rolls_back_and_cancel_does_not_publish() {
    let server = Server::start().await;
    let bytes = figura_package();
    let mut input = upload_input(&bytes, "PUBLIC");
    input["metadata"]["authors"] = json!(["Forged author"]);
    let reservation = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let body_path = reservation["relative_path"].as_str().unwrap();
    assert_eq!(
        server
            .request(Method::PUT, body_path, 1)
            .header("content-type", "application/octet-stream")
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let operation = input["operation_id"].as_str().unwrap();
    let result = server
        .request(
            Method::POST,
            &format!("/api/library/v1/uploads/{operation}/commit"),
            1,
        )
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 400);
    let asset = format!(
        "library_{}",
        reservation["model_id"].as_str().unwrap().replace('-', "")
    );
    assert!(
        server
            .state
            .storage
            .avatar(Uuid::from_u128(1), &asset, 1)
            .is_err()
    );
    let result = server
        .request(
            Method::DELETE,
            &format!("/api/library/v1/uploads/{operation}"),
            1,
        )
        .json(&json!({"operation_id": library::random_id()}))
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), 200);
    assert_eq!(
        server
            .request(Method::PUT, body_path, 1)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn upload_limits_changed_content_and_forged_uploader_are_rejected_before_publication() {
    let server = Server::start().await;
    let bytes = figura_package();
    let input = upload_input(&bytes, "PUBLIC");
    server
        .state
        .config
        .write()
        .await
        .model_library
        .max_upload_bytes = 1;
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/uploads", 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        413
    );
    server
        .state
        .config
        .write()
        .await
        .model_library
        .max_upload_bytes = 64 * 1024 * 1024;
    let mut forged = input.clone();
    forged["source_filename"] = json!("unsupported.exe");
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/uploads", 1)
            .json(&forged)
            .send()
            .await
            .unwrap()
            .status(),
        415
    );
    let mut forged = input.clone();
    forged["uploader_uuid"] = json!(Uuid::from_u128(2).to_string());
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/uploads", 1)
            .json(&forged)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let first = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let mut changed = input.clone();
    changed["metadata"]["display_name"] = json!("Changed");
    assert_eq!(
        server
            .request(Method::POST, "/api/library/v1/uploads", 1)
            .json(&changed)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
}

#[tokio::test]
async fn authenticated_lists_and_totals_apply_identical_policy_and_pending_rules() {
    let server = Server::start().await;
    server.seed("PUBLIC", true, "ACTIVE", true);
    server.seed("PRIVATE", true, "ACTIVE", true);
    server.seed("PRIVATE", true, "BLOCKED", true);
    server.seed("PUBLIC", false, "ACTIVE", false);
    let unauthenticated = server
        .client
        .get(format!("{}/api/library/v1/models", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), 401);
    assert_eq!(
        unauthenticated.json::<Value>().await.unwrap()["code"],
        "UNAUTHENTICATED"
    );
    for (player, scope, expected) in [
        (1, "public", 1),
        (1, "mine", 2),
        (2, "public", 1),
        (2, "mine", 0),
    ] {
        let response = server
            .request(
                Method::GET,
                &format!("/api/library/v1/models?scope={scope}"),
                player,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let page = response.json::<Value>().await.unwrap();
        assert_eq!(page["total"], expected);
        assert_eq!(page["items"].as_array().unwrap().len() as i64, expected);
    }
    let admin = server
        .request(Method::GET, "/api/library/v1/models?scope=admin", 1)
        .send()
        .await
        .unwrap();
    assert_eq!(admin.status(), 403);
    let invalid = server
        .request(Method::GET, "/api/library/v1/models?page_size=9", 1)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    let forged = server
        .request(Method::GET, "/api/library/v1/session", 1)
        .header("x-omnimorph-epoch", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), 401);
}

#[tokio::test]
async fn favorite_retries_are_unique_and_hidden_removal_does_not_return_metadata() {
    let server = Server::start().await;
    let id = server.seed("PUBLIC", true, "ACTIVE", true);
    let path = format!("/api/library/v1/models/{id}/favorite");
    let input = json!({"operation_id": library::random_id()});
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::PUT, &path, 2)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let detail = server
        .request(Method::GET, &format!("/api/library/v1/models/{id}"), 2)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(detail["favorites"], 1);
    assert!(
        detail["allowed_actions"]
            .as_array()
            .unwrap()
            .contains(&json!("download"))
    );
    server
        .state
        .storage
        .library_transaction(|transaction| {
            transaction
                .execute(
                    "UPDATE library_models SET visibility = 'PRIVATE' WHERE model_id = ?1",
                    [&id],
                )
                .map_err(library::sql_error)?;
            Ok(())
        })
        .unwrap();
    let response = server
        .request(Method::GET, "/api/library/v1/models?scope=favorites", 2)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(response["total"], 0);
    let response = server
        .request(Method::DELETE, &path, 2)
        .json(&json!({"operation_id": library::random_id()}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"favorited": false})
    );
}

#[tokio::test]
async fn patch_preserves_independent_options_and_conflicts_and_forged_identity_fail() {
    let server = Server::start().await;
    let id = server.seed("PUBLIC", true, "ACTIVE", true);
    let path = format!("/api/library/v1/models/{id}");
    let input = json!({"operation_id": library::random_id(), "expected_revision": 1, "visibility": "PRIVATE"});
    for _ in 0..2 {
        assert_eq!(
            server
                .request(Method::PATCH, &path, 1)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let response = server
        .request(Method::GET, &path, 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(response["visibility"], "PRIVATE");
    assert_eq!(response["requires_authorization"], true);
    assert_eq!(
        server
            .request(Method::GET, &path, 2)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let conflict = json!({"operation_id": library::random_id(), "expected_revision": 1, "display_name": "stale"});
    assert_eq!(
        server
            .request(Method::PATCH, &path, 1)
            .json(&conflict)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let forged = json!({"operation_id": library::random_id(), "expected_revision": 2, "uploader_uuid": Uuid::from_u128(2).to_string()});
    assert_eq!(
        server
            .request(Method::PATCH, &path, 1)
            .json(&forged)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
}

#[tokio::test]
async fn grants_resolve_only_verified_players_and_private_overrides_grants() {
    let server = Server::start().await;
    let id = server.seed("PRIVATE", true, "ACTIVE", true);
    let resolve = format!("/api/library/v1/players/resolve?model_id={id}&name=Player3");
    let response = server
        .request(Method::GET, &resolve, 1)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(response["items"][0]["uuid"], Uuid::from_u128(3).to_string());
    assert_eq!(
        server
            .request(Method::GET, &resolve, 3)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let path = format!("/api/library/v1/models/{id}/grants/{}", Uuid::from_u128(3));
    let input = json!({"operation_id": library::random_id(), "expected_revision": 1});
    assert_eq!(
        server
            .request(Method::PUT, &path, 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    server.state.user_manager.insert(
        Uuid::from_u128(3),
        "synthetic-token-3".into(),
        Userinfo {
            uuid: Uuid::from_u128(3),
            nickname: "RenamedPlayer".into(),
            ..Default::default()
        },
    ).unwrap();
    let model_path = format!("/api/library/v1/models/{id}");
    assert_eq!(server.request(Method::PATCH, &model_path, 1)
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 2, "visibility": "PUBLIC"}))
        .send().await.unwrap().status(), 200);
    assert_eq!(server.request(Method::GET, &model_path, 3).send().await.unwrap().status(), 200,
        "renaming a player must retain the UUID grant");
    let renamed_path = format!("/api/library/v1/players/resolve?model_id={id}&name=RenamedPlayer");
    let renamed = server.request(Method::GET, &renamed_path, 1).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(renamed["items"][0]["uuid"], Uuid::from_u128(3).to_string());
    server.state.user_manager.insert(
        Uuid::from_u128(2),
        "synthetic-token-2".into(),
        Userinfo {
            uuid: Uuid::from_u128(2),
            nickname: "RenamedPlayer".into(),
            ..Default::default()
        },
    ).unwrap();
    assert_eq!(server.request(Method::GET, &renamed_path, 1).send().await.unwrap().status(), 409,
        "ambiguous verified names must not select an arbitrary UUID");
    assert_eq!(server.request(Method::PATCH, &model_path, 1)
        .json(&json!({"operation_id": library::random_id(), "expected_revision": 3, "visibility": "PRIVATE"}))
        .send().await.unwrap().status(), 200);
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{id}"), 3)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[test]
fn metadata_limits_and_literal_text_budget_are_enforced() {
    let mut metadata = library::Metadata {
        display_name: "示例".into(),
        internal_name: "内部".into(),
        authors: vec!["作者".into()],
        model_type: "FIGURA".into(),
        category_id: "general".into(),
        description: String::new(),
        tags: vec![],
        visibility: Visibility::Public,
        requires_authorization: false,
    };
    let categories = vec!["general".into()];
    metadata.validate(&categories).unwrap();
    metadata.authors.push("作者".into());
    assert!(metadata.validate(&categories).is_err());
    metadata.authors.clear();
    metadata.display_name = "中".repeat(33);
    assert!(metadata.validate(&categories).is_err());
    metadata.display_name = "§kHidden".into();
    assert!(metadata.validate(&categories).is_err());
}
