use super::*;
use crate::{
    api::figura::SessionMessage, protocol::v1::Auth, service::event::CloudEvent,
    storage::outfit_apply::ApplyInput,
};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[tokio::test]
async fn outfit_events_java_adapters_use_the_real_rust_revision_channel() {
    if std::env::var_os("OMNIMORPH_CLIENT_CLASSPATH_FILE").is_none() {
        println!("SKIPPED Java outfit events: set OMNIMORPH_CLIENT_CLASSPATH_FILE");
        return;
    }
    let server = Server::start().await;
    crate::api::protobuf::tests::java_client::execute(
        &[
            "top.aihmc.omnimorph.backend2.library.OutfitEventsSmokeTest".into(),
            server.base.clone(),
            server.state.storage.instance_id(),
        ],
        server._directory.path(),
    )
    .await;
    wait_connections(&server, 2, 0).await;
}

pub(super) struct Peer {
    socket: Socket,
    sequence: u64,
    server_epoch: String,
}

async fn request(server: &Server, player: u128, protobuf: bool) -> axum::http::Request<()> {
    let mut request = (server.base.replacen("http://", "ws://", 1)
        + "/api/library/v1/outfit-events")
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert(
        "sec-websocket-protocol",
        super::super::outfit_events::SUBPROTOCOL.parse().unwrap(),
    );
    headers.insert("x-omnimorph-origin", server.base.parse().unwrap());
    let token = if protobuf {
        headers.insert("x-omnimorph-epoch", "77".parse().unwrap());
        server
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
            .unwrap()
            .session_id
    } else {
        headers.insert("x-omnimorph-auth-mode", "figura".parse().unwrap());
        format!("synthetic-token-{player}")
    };
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

impl Peer {
    pub(super) async fn connect(server: &Server, player: u128, protobuf: bool) -> Self {
        let (socket, response) =
            tokio_tungstenite::connect_async(request(server, player, protobuf).await)
                .await
                .unwrap();
        assert_eq!(
            response.headers()["sec-websocket-protocol"],
            super::super::outfit_events::SUBPROTOCOL
        );
        let mut peer = Self {
            socket,
            sequence: 0,
            server_epoch: String::new(),
        };
        peer.command("HELLO", vec![]).await;
        let hello = peer.next().await;
        assert_eq!(hello["type"], "HELLO_ACK");
        assert_eq!(
            hello["instance_id"],
            server.state.protocol_sessions.instance_id
        );
        peer
    }

    async fn command(&mut self, kind: &str, owners: Vec<Uuid>) {
        self.socket
            .send(Message::Text(
                json!({"schema_version": 1,
            "connection_epoch": 77, "type": kind, "owners": owners})
                .to_string(),
            ))
            .await
            .unwrap();
    }

    pub(super) async fn next(&mut self) -> Value {
        let message = tokio::time::timeout(Duration::from_secs(3), self.socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let Message::Text(text) = message else {
            panic!("Expected JSON notification: {message:?}");
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        self.sequence += 1;
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["connection_epoch"], 77);
        assert_eq!(value["sequence"], self.sequence);
        let epoch = value["server_epoch"].as_str().unwrap();
        epoch.parse::<u64>().unwrap();
        if self.server_epoch.is_empty() {
            self.server_epoch = epoch.to_owned();
        } else {
            assert_eq!(epoch, self.server_epoch);
        }
        value
    }

    pub(super) async fn subscribe(&mut self, owner: Uuid) -> Value {
        self.command("SUBSCRIBE", vec![owner]).await;
        assert_eq!(self.next().await["type"], "ACK");
        self.next().await
    }
}

async fn wait_connections(server: &Server, player: u128, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.state.session.count(Uuid::from_u128(player)) != expected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn outfit_events_both_credentials_receive_committed_revisions_and_policy_invalidations() {
    let server = Server::start().await;
    let owner = Uuid::from_u128(1);
    for protobuf in [false, true] {
        let mut peer = Peer::connect(&server, 2, protobuf).await;
        let baseline = peer.subscribe(owner).await;
        assert_eq!(baseline["type"], "CHANGED");
        assert_eq!(baseline["owner"], owner.to_string());
        let configuration = server.state.storage.outfit_current(owner).unwrap();
        let response = server
            .request(Method::PUT, "/api/library/v1/outfits/current", 1)
            .header("x-omnimorph-origin", &server.base)
            .json(&ApplyInput {
                operation_id: library::random_id().parse().unwrap(),
                expected_revision: configuration.revision,
                configuration,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        let receipt: Value = response.json().await.unwrap();
        let changed = peer.next().await;
        assert_eq!(changed["revision"], receipt["current_revision"]);
        assert!(changed.get("configuration").is_none());
        assert!(changed.get("resources").is_none());
        server.state.subscribes.invalidate_observers();
        assert_eq!(peer.next().await["revision"], receipt["current_revision"]);
        peer.command("UNSUBSCRIBE", vec![owner]).await;
        assert_eq!(peer.next().await["type"], "ACK");
        crate::service::equipment::changed(&server.state, owner).await;
        // A command barrier proves no unsubscribed notification precedes its response.
        peer.command("HEARTBEAT", vec![]).await;
        assert_eq!(peer.next().await["type"], "HEARTBEAT");
        peer.socket.close(None).await.unwrap();
        wait_connections(&server, 2, 0).await;
    }
}

#[tokio::test]
async fn outfit_events_reject_query_credentials_missing_origin_and_wrong_hello_scope() {
    let server = Server::start().await;
    let mut input = request(&server, 2, false).await;
    *input.uri_mut() = format!("{}?token=forbidden", input.uri()).parse().unwrap();
    assert!(tokio_tungstenite::connect_async(input).await.is_err());
    let mut input = request(&server, 2, false).await;
    input.headers_mut().remove("x-omnimorph-origin");
    assert!(tokio_tungstenite::connect_async(input).await.is_err());
    let mut input = request(&server, 2, false).await;
    input.headers_mut().remove("sec-websocket-protocol");
    assert!(tokio_tungstenite::connect_async(input).await.is_err());
    let (mut socket, _) = tokio_tungstenite::connect_async(request(&server, 2, true).await)
        .await
        .unwrap();
    socket
        .send(Message::Text(
            json!({"schema_version":1,"connection_epoch":78,
        "type":"HELLO"})
            .to_string(),
        ))
        .await
        .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(message, Message::Text(_) | Message::Close(_)));
    wait_connections(&server, 2, 0).await;
}

#[tokio::test]
async fn outfit_events_full_subscription_baseline_bypasses_queue_and_reconnects_from_current_state()
{
    let server = Server::start().await;
    let mut peer = Peer::connect(&server, 2, false).await;
    let owners: Vec<_> = (10..138).map(Uuid::from_u128).collect();
    peer.command("SUBSCRIBE", owners.clone()).await;
    assert_eq!(peer.next().await["type"], "ACK");
    for owner in &owners {
        let changed = peer.next().await;
        assert_eq!(changed["type"], "CHANGED");
        assert_eq!(changed["owner"], owner.to_string());
        assert_eq!(changed["revision"], 0);
    }
    peer.command("SUBSCRIBE", vec![Uuid::from_u128(138)]).await;
    assert_eq!(peer.next().await["type"], "RESYNC_REQUIRED");
    wait_connections(&server, 2, 0).await;
    let mut peer = Peer::connect(&server, 2, false).await;
    assert_eq!(peer.subscribe(Uuid::from_u128(1)).await["revision"], 0);
    peer.command("SUBSCRIBE", vec![owners[0], owners[0]]).await;
    assert_eq!(peer.next().await["type"], "ERROR");
    wait_connections(&server, 2, 0).await;
}

#[tokio::test]
async fn outfit_events_queue_overflow_retires_only_its_connection_and_does_not_forward_legacy_bytes(
) {
    let server = Server::start().await;
    let viewer = Uuid::from_u128(2);
    let mut peer = Peer::connect(&server, 2, false).await;
    let recipients = server.state.session.get(&viewer).unwrap();
    recipients
        .try_send(SessionMessage::Ping(b"privileged".to_vec()))
        .unwrap();
    recipients
        .try_send(SessionMessage::Event(CloudEvent::LegacyRaw(
            b"private".to_vec(),
        )))
        .unwrap();
    peer.command("HEARTBEAT", vec![]).await;
    assert_eq!(peer.next().await["type"], "HEARTBEAT");
    // A frame already in flight may arrive first; overflow must still force bounded recovery.
    let (other_sender, _other_receiver) = crate::service::queue::channel(32);
    let other_id = server
        .state
        .session
        .register_bounded(viewer, other_sender)
        .unwrap();
    for _ in 0..40 {
        let _ = recipients.try_send(SessionMessage::Event(CloudEvent::AvatarChanged {
            owner: viewer,
            tombstone: false,
        }));
    }
    let mut recovered = false;
    for _ in 0..33 {
        let message = peer.next().await;
        if message["type"] == "RESYNC_REQUIRED" {
            assert_eq!(message["code"], "SNAPSHOT_REQUIRED");
            recovered = true;
            break;
        }
        assert_eq!(message["type"], "CHANGED");
    }
    assert!(recovered);
    wait_connections(&server, 2, 1).await;
    assert!(server.state.session.close_signal(other_id).is_some());
    server.state.session.remove(other_id);
    let mut next = Peer::connect(&server, 2, true).await;
    next.command("HEARTBEAT", vec![]).await;
    assert_eq!(next.next().await["type"], "HEARTBEAT");
}

#[tokio::test]
async fn outfit_events_reject_malformed_commands_and_preserve_shared_protobuf_credentials() {
    let server = Server::start().await;
    let original = request(&server, 2, true).await;
    let session_id = original.headers()["authorization"]
        .to_str()
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap()
        .to_owned();
    let mut peer = Peer::connect(&server, 2, true).await;
    peer.socket.close(None).await.unwrap();
    wait_connections(&server, 2, 0).await;
    server
        .state
        .protocol_sessions
        .authorize(&server.state, &session_id, 77)
        .unwrap();

    for body in [
        json!({"schema_version":2,"connection_epoch":77,"type":"HEARTBEAT"}),
        json!({"schema_version":1,"connection_epoch":78,"type":"HEARTBEAT"}),
        json!({"schema_version":1,"connection_epoch":77,"type":"HEARTBEAT","token":"forbidden"}),
        json!({"schema_version":1,"connection_epoch":77,"type":"STATE","owners":[]}),
    ] {
        let mut peer = Peer::connect(&server, 3, false).await;
        peer.socket
            .send(Message::Text(body.to_string()))
            .await
            .unwrap();
        assert_eq!(peer.next().await["type"], "ERROR");
        wait_connections(&server, 3, 0).await;
    }
}

#[tokio::test]
async fn outfit_events_recheck_credentials_before_sending_and_share_account_budget() {
    let server = Server::start().await;
    let viewer = Uuid::from_u128(2);
    let mut peer = Peer::connect(&server, 2, false).await;
    peer.subscribe(Uuid::from_u128(1)).await;
    server
        .state
        .user_manager
        .ban(&Userinfo {
            uuid: viewer,
            ..Default::default()
        })
        .unwrap();
    server.state.subscribes.invalidate_observers();
    let message = tokio::time::timeout(Duration::from_secs(2), peer.socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(message, Message::Close(_)));
    wait_connections(&server, 2, 0).await;

    let mut peer = Peer::connect(&server, 3, false).await;
    while server
        .state
        .protocol_sessions
        .charge(Uuid::from_u128(3), 1)
        .is_ok()
    {}
    peer.command("HEARTBEAT", vec![]).await;
    let error = peer.next().await;
    assert_eq!(error["type"], "RESYNC_REQUIRED");
    assert_eq!(error["code"], "RATE_LIMITED");
    wait_connections(&server, 3, 0).await;
}

#[tokio::test]
async fn outfit_events_live_limits_change_is_an_invalidation_not_a_new_identity() {
    let server = Server::start().await;
    let mut peer = Peer::connect(&server, 2, false).await;
    server
        .state
        .config
        .write()
        .await
        .outfit
        .limits
        .max_instances = 1;
    // The production heartbeat checks hot-reloaded limits every 20 seconds.
    let message = tokio::time::timeout(Duration::from_secs(23), peer.socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let Message::Text(text) = message else {
        panic!("Expected limits invalidation");
    };
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["type"], "LIMITS_CHANGED");
    assert_eq!(value["sequence"], 2);
    assert_eq!(value["connection_epoch"], 77);
    assert_eq!(value["server_epoch"], peer.server_epoch);
    peer.sequence = 2;
    assert_eq!(peer.next().await["type"], "HEARTBEAT");
    peer.socket.close(None).await.unwrap();
    wait_connections(&server, 2, 0).await;
}
