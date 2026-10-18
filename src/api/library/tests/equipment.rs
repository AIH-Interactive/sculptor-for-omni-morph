use super::*;

fn equip_input(revision: u64) -> Value {
    json!({
        "operation_id": library::random_id(),
        "model_revision": 1,
        "expected_equipment_revision": revision,
    })
}

#[tokio::test]
async fn session_exposes_only_the_authenticated_players_current_equipment_revision() {
    let server = Server::start().await;
    let read = |player| server.request(Method::GET, "/api/library/v1/session", player);
    let initial = read(1).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(initial["equipment_revision"], 0);
    assert_eq!(initial["uuid"], Uuid::from_u128(1).to_string());
    let (id, _) = server.upload_model("PRIVATE").await;
    let path = format!("/api/library/v1/models/{id}/equip");
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&equip_input(0))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let equipped = read(1).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(equipped["equipment_revision"], 1);
    assert_eq!(equipped["equipment_model_id"], id);
    assert_eq!(equipped["equipment_model_revision"], 1);
    assert_eq!(equipped["show_private_equipment"], false);
    let mut visible = equip_input(1);
    visible["show_private_equipment"] = json!(true);
    assert_eq!(server.request(Method::POST, &path, 1).json(&visible)
        .send().await.unwrap().status(), 200);
    let saved = read(1).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(saved["show_private_equipment"], true);
    assert_eq!(saved["equipment_model_id"], id);
    let other = read(2).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(other["equipment_revision"], 0);
    assert_eq!(other["uuid"], Uuid::from_u128(2).to_string());
    assert!(other["equipment_model_id"].is_null());
    assert_eq!(other["show_private_equipment"], false);
    assert!(other.get("model_id").is_none());
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&equip_input(0))
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
}

#[tokio::test]
async fn legacy_download_uses_source_equipment_instead_of_a_stale_temporary_slot() {
    let server = Server::start().await;
    let player = Uuid::from_u128(99);
    server
        .state
        .user_manager
        .insert(
            player,
            "synthetic-token-99".into(),
            Userinfo {
                uuid: player,
                nickname: "Synthetic99".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let (id, bytes) = server.upload_model("PUBLIC").await;
    let path = format!("/api/library/v1/models/{id}/equip");
    assert_eq!(
        server
            .request(Method::POST, &path, 99)
            .json(&equip_input(0))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let temporary = crate::service::resource::legacy_path(player, true);
    tokio::fs::create_dir_all(temporary.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&temporary, b"synthetic stale temp data")
        .await
        .unwrap();
    let response = server
        .client
        .get(format!("{}/api/{player}/avatar", server.base))
        .header("token", "synthetic-token-99")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_slice());
    assert_eq!(
        tokio::fs::read(&temporary).await.unwrap(),
        b"synthetic stale temp data"
    );
    tokio::fs::remove_file(temporary).await.unwrap();
}

#[tokio::test]
async fn resource_body_stops_at_the_next_chunk_when_policy_or_session_is_revoked() {
    use futures_util::StreamExt;
    for revoke_session in [false, true] {
        let server = Server::start().await;
        let (id, _) = server.upload_model("PUBLIC").await;
        let path = format!("/api/library/v1/models/{id}/equip");
        let response = server
            .request(Method::POST, &path, 2)
            .json(&equip_input(0))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let equipped = server.state.storage.equipped(Uuid::from_u128(2)).unwrap();
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
        // A bounded byte source makes chunk polling deterministic; package parsing
        // is covered by the upload fixture, not by these synthetic padding bytes.
        let file_path = server._directory.path().join("stream-fixture.bin");
        tokio::fs::write(&file_path, vec![7; 96 * 1024])
            .await
            .unwrap();
        let file = tokio::fs::File::open(&file_path).await.unwrap();
        let body = crate::service::resource::guarded_body(
            server.state.clone(),
            file,
            96 * 1024,
            equipped.primary,
            Uuid::from_u128(2),
            move |state| {
                state
                    .protocol_sessions
                    .authorize(state, &scope.session_id, 77)
                    .map(|session| session.owner)
            },
        )
        .unwrap();
        let mut stream = body.into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().len(), 32 * 1024);
        if revoke_session {
            let user = server
                .state
                .user_manager
                .get_by_uuid(&Uuid::from_u128(2))
                .unwrap()
                .clone();
            server.state.user_manager.ban(&user).unwrap();
        } else {
            let patch = json!({"operation_id": library::random_id(), "expected_revision": 2, "visibility": "PRIVATE"});
            assert_eq!(
                server
                    .request(Method::PATCH, &format!("/api/library/v1/models/{id}"), 1)
                    .json(&patch)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                200
            );
        }
        assert!(stream.next().await.unwrap().is_err());
    }
}

#[tokio::test]
async fn blocking_overrides_an_existing_private_render_opt_in_and_rejects_reopening() {
    let server = Server::start().await;
    let (id, _) = server.upload_model("PRIVATE").await;
    let path = format!("/api/library/v1/models/{id}/equip");
    let mut input = equip_input(0);
    input["show_private_equipment"] = json!(true);
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_profiles(&server, 2, true).await;
    let moderator = crate::service::library_policy::Actor {
        uuid: Uuid::from_u128(4),
        admin_context: true,
        granted: false,
    };
    let block = serde_json::from_value(json!({
        "operation_id": library::random_id(), "expected_revision": 2,
        "reason": "synthetic moderation",
    }))
    .unwrap();
    server
        .state
        .storage
        .library_block(moderator, &id, &block, true)
        .unwrap();
    assert_profiles(&server, 2, false).await;
    assert_profiles(&server, 1, true).await;
    let mut reopen = equip_input(1);
    reopen["show_private_equipment"] = json!(true);
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&reopen)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let input = serde_json::from_value(reopen).unwrap();
    let owner_admin = crate::service::library_policy::Actor {
        uuid: Uuid::from_u128(1),
        admin_context: true,
        granted: false,
    };
    assert_eq!(
        server
            .state
            .storage
            .library_equip(owner_admin, &id, &input)
            .unwrap_err(),
        ErrorCode::Forbidden
    );
    assert_eq!(
        server
            .state
            .storage
            .equipped(Uuid::from_u128(1))
            .unwrap()
            .revision,
        1
    );
}

pub(super) async fn assert_profiles(server: &Server, observer: u128, visible: bool) {
    use crate::protocol::v1::{Auth, Envelope, ProfileRequest, envelope};
    use prost::Message;
    let scope = server
        .state
        .protocol_sessions
        .issue(
            &server.state,
            &Auth {
                token: format!("synthetic-token-{observer}"),
                instance_id: server.state.protocol_sessions.instance_id.clone(),
                ..Default::default()
            },
            77,
        )
        .unwrap();
    let request = Envelope {
        major: 1,
        request_id: 1,
        connection_epoch: 77,
        session_id: scope.session_id.clone(),
        body: Some(envelope::Body::ProfileRequest(ProfileRequest {
            owner: Uuid::from_u128(1).to_string(),
        })),
    };
    let response = server
        .client
        .post(format!("{}/api/protobuf/v1/rpc", server.base))
        .header("content-type", crate::api::protobuf::CONTENT_TYPE)
        .bearer_auth(&scope.session_id)
        .body(request.encode_to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let message = Envelope::decode(response.bytes().await.unwrap()).unwrap();
    let Some(envelope::Body::Profile(profile)) = message.body else {
        panic!("Expected a profile projection");
    };
    let equipment = profile.equipped.unwrap();
    assert_eq!(equipment.primary.is_some(), visible);
    assert_eq!(equipment.figura_compatibility.is_some(), visible);
    let response = server
        .client
        .get(format!("{}/api/{}", server.base, Uuid::from_u128(1)))
        .header("token", format!("synthetic-token-{observer}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let profile = response.json::<Value>().await.unwrap();
    assert_eq!(
        profile["equipped"].as_array().unwrap().len(),
        usize::from(visible)
    );
    if visible {
        assert_eq!(profile["equipped"][0]["hash"].as_str().unwrap().len(), 64);
    }
    if observer != 1 {
        let response = server
            .client
            .get(format!("{}/api/{}/avatar", server.base, Uuid::from_u128(1)))
            .header("token", format!("synthetic-token-{observer}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), if visible { 200 } else { 404 });
        if visible {
            assert_eq!(
                response.bytes().await.unwrap().as_ref(),
                figura_package().as_slice()
            );
        }
    }
}

#[tokio::test]
async fn source_equipment_and_its_intent_and_operation_survive_service_restart() {
    let mut server = Server::start().await;
    let (id, _) = server.upload_model("PRIVATE").await;
    let path = format!("/api/library/v1/models/{id}/equip");
    let mut input = equip_input(0);
    input["show_private_equipment"] = json!(true);
    let response = server
        .request(Method::POST, &path, 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let first = response.json::<Value>().await.unwrap();
    super::lifecycle::restart(&mut server).await;
    assert_profiles(&server, 1, true).await;
    assert_profiles(&server, 2, true).await;
    let response = server
        .request(Method::POST, &path, 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), first);
    assert_eq!(
        server
            .state
            .storage
            .equipped(Uuid::from_u128(1))
            .unwrap()
            .revision,
        1
    );
}

#[tokio::test]
async fn concurrent_equips_enforce_the_players_equipment_revision() {
    let server = Server::start().await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let path = format!("/api/library/v1/models/{id}/equip");
    let first = server.request(Method::POST, &path, 2).json(&equip_input(0));
    let second = server.request(Method::POST, &path, 2).json(&equip_input(0));
    let (first, second) = tokio::join!(first.send(), second.send());
    let mut statuses = [
        first.unwrap().status().as_u16(),
        second.unwrap().status().as_u16(),
    ];
    statuses.sort();
    assert_eq!(statuses, [200, 409]);
    assert_eq!(
        server
            .state
            .storage
            .equipped(Uuid::from_u128(2))
            .unwrap()
            .revision,
        1
    );
}

#[tokio::test]
async fn observer_grant_cannot_keep_a_revoked_wearers_alias_readable() {
    let server = Server::start().await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let detail = format!("/api/library/v1/models/{id}");
    let path = format!("{detail}/equip");
    let patch = json!({"operation_id": library::random_id(), "expected_revision": 2, "requires_authorization": true});
    assert_eq!(
        server
            .request(Method::PATCH, &detail, 1)
            .json(&patch)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let request = equip_input(0);
    assert_eq!(
        server
            .request(Method::POST, &path, 2)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    for (player, revision) in [(2, 3), (3, 4)] {
        let grant = format!("{detail}/grants/{}", Uuid::from_u128(player));
        let input = json!({"operation_id": library::random_id(), "expected_revision": revision});
        assert_eq!(
            server
                .request(Method::PUT, &grant, 1)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let response = server
        .request(Method::POST, &path, 2)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let equipped = response.json::<Value>().await.unwrap();
    let scope = server
        .state
        .protocol_sessions
        .issue(
            &server.state,
            &crate::protocol::v1::Auth {
                token: "synthetic-token-3".into(),
                instance_id: server.state.protocol_sessions.instance_id.clone(),
                ..Default::default()
            },
            77,
        )
        .unwrap();
    let url = format!(
        "{}/api/protobuf/v1/resources/{}/{}/1",
        server.base,
        Uuid::from_u128(2),
        equipped["asset_id"].as_str().unwrap()
    );
    let download = || {
        server
            .client
            .get(&url)
            .bearer_auth(&scope.session_id)
            .header("x-omnimorph-epoch", "77")
    };
    assert_eq!(download().send().await.unwrap().status(), 200);
    let grant = format!("{detail}/grants/{}", Uuid::from_u128(2));
    let revoke = json!({"operation_id": library::random_id(), "expected_revision": 5});
    assert_eq!(
        server
            .request(Method::DELETE, &grant, 1)
            .json(&revoke)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(download().send().await.unwrap().status(), 403);
    assert_eq!(
        server
            .request(Method::POST, &path, 2)
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    // Revocation denies new reads without deleting the wearer's existing local selection.
    assert!(
        server
            .state
            .storage
            .equipped(Uuid::from_u128(2))
            .unwrap()
            .primary
            .is_some()
    );
}

#[tokio::test]
async fn library_equipment_alias_is_atomic_idempotent_and_retains_its_source() {
    let server = Server::start().await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let path = format!("/api/library/v1/models/{id}/equip");
    let input = equip_input(0);
    let response = server
        .request(Method::POST, &path, 2)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let first = response.json::<Value>().await.unwrap();
    assert_eq!(first["equipment_revision"], 1);
    assert_eq!(first["owner"], Uuid::from_u128(2).to_string());
    assert_eq!(first["show_private_equipment"], false);
    let alias = first["asset_id"].as_str().unwrap();
    let source: String = server
        .state
        .storage
        .library_read(|database| {
            database
                .query_row(
                    "SELECT source_model_id FROM library_resource_bindings
         WHERE resource_owner = ?1 AND asset_id = ?2 AND revision = 1",
                    params![Uuid::from_u128(2).to_string(), alias],
                    |row| row.get(0),
                )
                .map_err(library::sql_error)
        })
        .unwrap();
    assert_eq!(source, id);
    let replay = server
        .request(Method::POST, &path, 2)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(replay.json::<Value>().await.unwrap(), first);
    assert_eq!(
        server
            .state
            .storage
            .equipped(Uuid::from_u128(2))
            .unwrap()
            .revision,
        1
    );
    let conflict = equip_input(9);
    assert_eq!(
        server
            .request(Method::POST, &path, 2)
            .json(&conflict)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let mut changed = input.clone();
    changed["expected_equipment_revision"] = json!(1);
    assert_eq!(
        server
            .request(Method::POST, &path, 2)
            .json(&changed)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(
        server
            .state
            .storage
            .equip(
                Uuid::from_u128(2),
                &crate::protocol::v1::Equip {
                    operation_id: library::random_id(),
                    asset_id: alias.into(),
                    asset_revision: 1,
                    expected_revision: 1,
                    ..Default::default()
                }
            )
            .unwrap_err(),
        ErrorCode::Forbidden
    );

    server
        .state
        .storage
        .library_transaction(|transaction| {
            transaction.execute_batch(
            "CREATE TRIGGER reject_equipment_intent BEFORE INSERT ON library_equipment_settings
             BEGIN SELECT RAISE(ABORT, 'synthetic equipment failure'); END;"
        ).map_err(library::sql_error)
        })
        .unwrap();
    assert_eq!(
        server
            .request(Method::POST, &path, 3)
            .json(&equip_input(0))
            .send()
            .await
            .unwrap()
            .status(),
        500
    );
    let equipment = server.state.storage.equipped(Uuid::from_u128(3)).unwrap();
    assert_eq!(equipment.revision, 0);
    assert!(equipment.primary.is_none());
    let bound: bool = server
        .state
        .storage
        .library_read(|database| {
            database.query_row(
        "SELECT EXISTS(SELECT 1 FROM library_resource_bindings WHERE resource_owner = ?1)",
        [Uuid::from_u128(3).to_string()], |row| row.get(0),
    ).map_err(library::sql_error)
        })
        .unwrap();
    assert!(!bound);
}

#[tokio::test]
async fn private_equipment_intent_defaults_closed_and_updates_without_opening_catalog() {
    let server = Server::start().await;
    let (id, bytes) = server.upload_model("PRIVATE").await;
    let path = format!("/api/library/v1/models/{id}/equip");
    assert_eq!(
        server
            .request(Method::POST, &path, 2)
            .json(&equip_input(0))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let response = server
        .request(Method::POST, &path, 1)
        .json(&equip_input(0))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let first = response.json::<Value>().await.unwrap();
    assert_eq!(first["show_private_equipment"], false);
    assert_profiles(&server, 2, false).await;
    assert_profiles(&server, 1, true).await;
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
    let url = format!(
        "{}/api/protobuf/v1/resources/{}/{}/1",
        server.base,
        Uuid::from_u128(1),
        first["asset_id"].as_str().unwrap()
    );
    let download = || {
        server
            .client
            .get(&url)
            .bearer_auth(&scope.session_id)
            .header("x-omnimorph-epoch", "77")
    };
    assert_eq!(download().send().await.unwrap().status(), 404);
    let mut input = equip_input(1);
    input["show_private_equipment"] = json!(true);
    let response = server
        .request(Method::POST, &path, 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let second = response.json::<Value>().await.unwrap();
    assert_eq!(second["equipment_revision"], 2);
    assert_profiles(&server, 2, true).await;
    assert_eq!(second["asset_id"], first["asset_id"]);
    let response = download().send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_slice());
    let show: bool = server
        .state
        .storage
        .library_read(|database| {
            database
                .query_row(
                    "SELECT show_private_equipment FROM library_equipment_settings
         WHERE resource_owner = ?1 AND asset_id = ?2",
                    params![
                        Uuid::from_u128(1).to_string(),
                        second["asset_id"].as_str().unwrap()
                    ],
                    |row| row.get(0),
                )
                .map_err(library::sql_error)
        })
        .unwrap();
    assert!(show);
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{id}"), 2)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let close = equip_input(2);
    let response = server
        .request(Method::POST, &path, 1)
        .json(&close)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_profiles(&server, 2, false).await;
    assert_eq!(download().send().await.unwrap().status(), 404);
    assert_eq!(
        response.json::<Value>().await.unwrap()["equipment_revision"],
        3
    );
    assert_eq!(
        server
            .state
            .storage
            .equipped(Uuid::from_u128(1))
            .unwrap()
            .revision,
        3
    );
    let mut reopen = equip_input(3);
    reopen["show_private_equipment"] = json!(true);
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&reopen)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    server
        .state
        .storage
        .equip(
            Uuid::from_u128(1),
            &crate::protocol::v1::Equip {
                operation_id: library::random_id(),
                clear: true,
                expected_revision: 4,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(download().send().await.unwrap().status(), 404);
}
