use super::*;
use crate::{
    api::figura::websocket::{C2SMessage, S2CMessage},
    protocol::v1::{envelope::Body, *},
};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use std::{collections::VecDeque, time::Duration};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message as WsMessage, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn binary(socket: &mut Socket) -> Vec<u8> {
    let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let WsMessage::Binary(bytes) = message else {
        panic!("Expected a binary protocol frame");
    };
    bytes
}

struct Peer {
    socket: Socket,
    scope: AuthResult,
    sequence: u64,
    pending: VecDeque<Envelope>,
}

impl Peer {
    async fn connect(server: &Server, player: u128) -> Self {
        let scope = server
            .state
            .protocol_sessions
            .issue(
                &server.state,
                &Auth {
                    token: format!("synthetic-token-{player}"),
                    instance_id: server.state.protocol_sessions.instance_id.clone(),
                    ..Default::default()
                },
                77,
            )
            .unwrap();
        let mut request = (server.base.replacen("http://", "ws://", 1) + "/ws/protobuf/v1")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "sec-websocket-protocol",
            crate::api::protobuf::SUBPROTOCOL.parse().unwrap(),
        );
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", scope.session_id).parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("x-omnimorph-epoch", "77".parse().unwrap());
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let mut peer = Self {
            socket,
            scope,
            sequence: 0,
            pending: VecDeque::new(),
        };
        let body = peer
            .command(Body::Hello(Hello {
                supported_majors: vec![1],
                client_version: "library-test".into(),
                capabilities: vec![1, 2, 3, 4],
            }))
            .await;
        assert!(matches!(body, Body::HelloAck(_)));
        peer
    }

    async fn command(&mut self, body: Body) -> Body {
        self.sequence += 1;
        let message = Envelope {
            major: 1,
            request_id: self.sequence,
            session_id: self.scope.session_id.clone(),
            connection_epoch: 77,
            body: Some(body),
        };
        self.socket
            .send(WsMessage::Binary(message.encode_to_vec()))
            .await
            .unwrap();
        for _ in 0..32 {
            let response = Envelope::decode(binary(&mut self.socket).await.as_slice()).unwrap();
            if response.request_id == self.sequence {
                return response.body.unwrap();
            }
            assert!(self.pending.len() < 32);
            self.pending.push_back(response);
        }
        panic!("Missing matching command response");
    }

    async fn next(&mut self) -> Body {
        match self.pending.pop_front() {
            Some(message) => message.body.unwrap(),
            None => Envelope::decode(binary(&mut self.socket).await.as_slice())
                .unwrap()
                .body
                .unwrap(),
        }
    }

    async fn subscribe(&mut self, owner: Uuid) {
        assert!(matches!(
            self.command(Body::Subscribe(Subscribe {
                owners: vec![owner.to_string()],
                cursors: vec![],
            }))
            .await,
            Body::Ack(_)
        ));
    }
}

async fn legacy(server: &Server, player: u128, ysm: bool) -> Socket {
    let mut request = (server.base.replacen("http://", "ws://", 1) + "/ws")
        .into_client_request()
        .unwrap();
    if ysm {
        request
            .headers_mut()
            .insert("x-omnimorph-models", "ysm".parse().unwrap());
    }
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let bytes: Vec<u8> = C2SMessage::Token(format!("synthetic-token-{player}").into_bytes()).into();
    socket.send(WsMessage::Binary(bytes)).await.unwrap();
    assert_eq!(binary(&mut socket).await, [0]);
    let bytes: Vec<u8> = C2SMessage::Sub(Uuid::from_u128(1)).into();
    socket.send(WsMessage::Binary(bytes)).await.unwrap();
    socket
}

#[tokio::test]
async fn authorized_observer_matrix_and_revocation_do_not_inherit_admin_rights() {
    let server = Server::start().await;
    let capabilities = server
        .request(Method::GET, "/api/library/v1/capabilities", 1)
        .send()
        .await
        .unwrap();
    assert_eq!(capabilities.status(), 200);
    assert_eq!(capabilities.json::<Value>().await.unwrap()["enabled"], true);
    super::admin::configure(&server).await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let owner = Uuid::from_u128(1);
    let detail = format!("/api/library/v1/models/{id}");
    let patch = json!({
        "operation_id": library::random_id(), "expected_revision": 2,
        "requires_authorization": true,
    });
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
    let grant_path = format!("{detail}/grants/{}", Uuid::from_u128(2));
    let grant = json!({"operation_id": library::random_id(), "expected_revision": 3});
    assert_eq!(
        server
            .request(Method::PUT, &grant_path, 1)
            .json(&grant)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let equip = json!({
        "operation_id": library::random_id(), "model_revision": 1,
        "expected_equipment_revision": 0,
    });
    assert_eq!(
        server
            .request(Method::POST, &format!("{detail}/equip"), 1)
            .json(&equip)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    // A is the uploader, B has a grant, C has none, D is configured as an administrator.
    // Administrator catalog access must never become an observer's model-use grant.
    for (player, visible) in [(1, true), (2, true), (3, false), (4, false)] {
        super::equipment::assert_profiles(&server, player, visible).await;
        let mut peer = Peer::connect(&server, player).await;
        let response = peer
            .command(Body::ProfileRequest(ProfileRequest {
                owner: owner.to_string(),
            }))
            .await;
        let Body::Profile(profile) = response else {
            panic!("Expected an observer-specific WebSocket profile");
        };
        let projected = profile.equipped.unwrap();
        assert_eq!(projected.primary.is_some(), visible);
        assert_eq!(projected.figura_compatibility.is_some(), visible);
        peer.socket.close(None).await.unwrap();
    }

    let mut granted = Peer::connect(&server, 2).await;
    granted.subscribe(owner).await;
    let mut old = legacy(&server, 2, false).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.state.subscribes.publisher(owner).receiver_count() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let revoke = json!({"operation_id": library::random_id(), "expected_revision": 4});
    assert_eq!(
        server
            .request(Method::DELETE, &grant_path, 1)
            .json(&revoke)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let Body::AvatarChanged(change) = granted.next().await else {
        panic!("Revoking an observer grant must invalidate its current model");
    };
    assert!(change.tombstone);
    assert!(change.equipped.unwrap().primary.is_none());
    assert!(matches!(
        S2CMessage::try_from(binary(&mut old).await.as_slice()).unwrap(),
        S2CMessage::Event(value) if value == owner
    ));
    super::equipment::assert_profiles(&server, 2, false).await;
    super::equipment::assert_profiles(&server, 1, true).await;
    granted.socket.close(None).await.unwrap();
    old.close(None).await.unwrap();
}

#[tokio::test]
async fn public_figura_change_and_body_are_observable_without_ysm_state() {
    let server = Server::start().await;
    let (model, bytes) = server.upload_model("PUBLIC").await;
    let owner = Uuid::from_u128(1);
    let mut viewer = Peer::connect(&server, 2).await;
    viewer.subscribe(owner).await;
    let response = server
        .request(
            Method::POST,
            &format!("/api/library/v1/models/{model}/equip"),
            1,
        )
        .json(
            &json!({"operation_id": library::random_id(), "model_revision": 1,
            "expected_equipment_revision": 0}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let Body::AvatarChanged(change) = viewer.next().await else {
        panic!("Figura must announce its asset without a YSM snapshot");
    };
    let equipped = change.equipped.unwrap();
    let figura = equipped.figura_compatibility.unwrap();
    assert_eq!(figura.model_type, 1);
    assert_eq!(figura.revision, 1);
    assert_eq!(figura.raw_length, bytes.len() as u64);
    assert_eq!(figura.raw_sha256, crate::storage::sha256(&bytes));
    let response = server
        .client
        .get(format!("{}/api/{owner}/avatar", server.base))
        .header("token", "synthetic-token-2")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_slice());
    viewer.socket.close(None).await.unwrap();
}

#[tokio::test]
async fn existing_observers_receive_private_visibility_removal_and_opt_in_changes() {
    let server = Server::start().await;
    let (id, _) = server.upload_model("PUBLIC").await;
    let owner = Uuid::from_u128(1);
    let mut viewer = Peer::connect(&server, 3).await;
    viewer.subscribe(owner).await;
    let mut old = legacy(&server, 2, false).await;
    // An ordinary ping confirms that both subscriptions have been installed.
    let ping = crate::service::event::CloudEvent::FiguraPing {
        owner,
        function_id: 42,
        echo: false,
        arguments: vec![7],
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.state.subscribes.publisher(owner).receiver_count() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server
        .state
        .subscribes
        .publisher(owner)
        .send(ping.clone())
        .unwrap();
    assert!(matches!(viewer.next().await, Body::FiguraPing(_)));
    assert!(matches!(
        S2CMessage::try_from(binary(&mut old).await.as_slice()).unwrap(),
        S2CMessage::Ping(..)
    ));
    let path = format!("/api/library/v1/models/{id}");
    let equip = format!("{path}/equip");
    let input = json!({"operation_id": library::random_id(), "model_revision": 1, "expected_equipment_revision": 0});
    assert_eq!(
        server
            .request(Method::POST, &equip, 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let Body::AvatarChanged(change) = viewer.next().await else {
        panic!("Missing equipment event");
    };
    assert!(change.equipped.unwrap().primary.is_some());
    assert!(
        matches!(S2CMessage::try_from(binary(&mut old).await.as_slice()).unwrap(), S2CMessage::Event(value) if value == owner)
    );
    let patch = json!({"operation_id": library::random_id(), "expected_revision": 2, "visibility": "PRIVATE"});
    assert_eq!(
        server
            .request(Method::PATCH, &path, 1)
            .json(&patch)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let Body::AvatarChanged(change) = viewer.next().await else {
        panic!("Missing revocation event");
    };
    assert!(change.tombstone);
    let equipment = change.equipped.unwrap();
    assert!(equipment.primary.is_none() && equipment.figura_compatibility.is_none());
    assert!(
        matches!(S2CMessage::try_from(binary(&mut old).await.as_slice()).unwrap(), S2CMessage::Event(value) if value == owner)
    );
    server.state.subscribes.publisher(owner).send(ping).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), viewer.next())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(150), old.next())
            .await
            .is_err()
    );
    let input = json!({"operation_id": library::random_id(), "model_revision": 1, "expected_equipment_revision": 1, "show_private_equipment": true});
    assert_eq!(
        server
            .request(Method::POST, &equip, 1)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        matches!(viewer.next().await, Body::AvatarChanged(change) if !change.tombstone && change.equipped.as_ref().unwrap().primary.is_some())
    );
    assert!(
        matches!(S2CMessage::try_from(binary(&mut old).await.as_slice()).unwrap(), S2CMessage::Event(value) if value == owner)
    );
    assert!(
        server
            .state
            .storage
            .equipped(owner)
            .unwrap()
            .primary
            .is_some()
    );
    viewer.socket.close(None).await.unwrap();
    old.close(None).await.unwrap();
}

pub(super) async fn encrypted_model(server: &Server) -> String {
    // Synthetic ciphertext verifies transport/policy only, not model rendering.
    let (bytes, content) = crate::service::package::tests::fixture(true, false);
    let mut input = upload_input(&bytes, "PRIVATE");
    input["metadata"]["model_type"] = json!("YSM_ENCRYPTED");
    input["content_sha256"] = json!(faster_hex::hex_string(&content));
    input["schema_sha256"] = json!("c".repeat(64));
    input["source_filename"] = json!("synthetic.ysm");
    input["metadata_source"] = json!("encrypted_manifest");
    let response = server
        .request(Method::POST, "/api/library/v1/uploads", 1)
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let reservation = response.json::<Value>().await.unwrap();
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
        200
    );
    reservation["model_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn private_ysm_recovery_and_queued_state_stop_after_opt_out_in_both_protocols() {
    let server = Server::start().await;
    let model = encrypted_model(&server).await;
    let path = format!("/api/library/v1/models/{model}/equip");
    let equip = json!({"operation_id": library::random_id(), "model_revision": 1,
        "expected_equipment_revision": 0, "show_private_equipment": true});
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&equip)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let owner = Uuid::from_u128(1);
    let avatar = server
        .state
        .storage
        .equipped(owner)
        .unwrap()
        .primary
        .unwrap();
    let snapshot = YsmSnapshot {
        identity: Some(YsmIdentity {
            owner: owner.to_string(),
            asset_revision: avatar.revision,
            publisher_session: Uuid::from_u128(50).to_string(),
            generation: 1,
            base_sequence: -1,
            content_sha256: avatar.content_sha256,
            schema_sha256: avatar.schema_sha256,
            ..Default::default()
        }),
        state: Some(YsmState {
            texture: Some(-1),
            controls: Some(Controls::default()),
            actions: Some(Actions::default()),
            controllers: Some(Controllers::default()),
            playbacks: Some(Playbacks::default()),
            display: None,
        }),
    };
    let mut host = Peer::connect(&server, 1).await;
    assert!(matches!(
        host.command(Body::YsmSnapshot(snapshot.clone())).await,
        Body::Ack(_)
    ));
    let mut viewer = Peer::connect(&server, 3).await;
    viewer.subscribe(owner).await;
    assert!(matches!(viewer.next().await, Body::YsmSnapshot(_)));
    let mut old = legacy(&server, 2, true).await;
    let mut fragments = crate::service::ysm::legacy::Reassembly::default();
    loop {
        let message = binary(&mut old).await;
        let S2CMessage::Ping(sender, function, _, packet) =
            S2CMessage::try_from(message.as_slice()).unwrap()
        else {
            panic!("Expected legacy YSM recovery");
        };
        assert_eq!(sender, owner);
        assert_eq!(function, crate::service::ysm::legacy::PING_ID);
        if let Some(frame) = fragments.accept(owner, &packet).unwrap() {
            let frame = crate::service::ysm::legacy::decode(&frame, avatar.revision).unwrap();
            assert_eq!(frame.identity.owner, owner.to_string());
            assert_eq!(frame.state.texture, Some(-1));
            break;
        }
    }
    assert!(matches!(
        viewer
            .command(Body::SnapshotRequest(SnapshotRequest {
                owner: owner.to_string(),
                ..Default::default()
            }))
            .await,
        Body::YsmSnapshot(_)
    ));
    let close = json!({"operation_id": library::random_id(), "model_revision": 1,
        "expected_equipment_revision": 1, "show_private_equipment": false});
    assert_eq!(
        server
            .request(Method::POST, &path, 1)
            .json(&close)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(matches!(viewer.next().await, Body::AvatarChanged(change) if change.tombstone));
    assert!(
        matches!(S2CMessage::try_from(binary(&mut old).await.as_slice()).unwrap(), S2CMessage::Event(value) if value == owner)
    );
    assert!(
        matches!(viewer.command(Body::SnapshotRequest(SnapshotRequest {
        owner: owner.to_string(), ..Default::default()
    })).await, Body::Error(error) if error.code == ErrorCode::NotFound as i32)
    );
    assert!(matches!(
        viewer
            .command(Body::Resume(Resume {
                owner: owner.to_string(),
                revision: avatar.revision,
                server_epoch: server.state.protocol_sessions.server_epoch,
                publisher_session: Uuid::from_u128(50).to_string(),
                generation: 1,
                ..Default::default()
            }))
            .await,
        Body::Ack(_)
    ));
    server
        .state
        .subscribes
        .publisher(owner)
        .send(crate::service::event::CloudEvent::Ysm {
            owner,
            body: Body::YsmSnapshot(snapshot),
            expires_at: None,
        })
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), viewer.next())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(150), old.next())
            .await
            .is_err()
    );
    assert_eq!(
        server
            .request(Method::GET, &format!("/api/library/v1/models/{model}"), 3)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    for socket in [&mut host.socket, &mut viewer.socket, &mut old] {
        socket.close(None).await.unwrap();
    }
}
