use crate::{
    api::figura::websocket::{C2SMessage, S2CMessage},
    auth::Userinfo,
    AppState,
};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{tungstenite::Message, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn send(socket: &mut Socket, message: C2SMessage) {
    let bytes: Vec<u8> = message.into();
    socket.send(Message::Binary(bytes)).await.unwrap();
}

async fn receive(socket: &mut Socket) -> Vec<u8> {
    let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("WS response timeout")
        .expect("WS closed")
        .unwrap();
    assert!(message.is_binary(), "Expected binary legacy frame");
    message.into_data()
}

async fn connect(base: &str, token: &str) -> Socket {
    let address = base.replacen("http://", "ws://", 1) + "/ws";
    let (mut socket, _) = tokio_tungstenite::connect_async(address).await.unwrap();
    send(&mut socket, C2SMessage::Token(token.as_bytes().to_vec())).await;
    assert_eq!(receive(&mut socket).await, vec![0]);
    socket
}

async fn barrier(socket: &mut Socket, owner: Uuid) {
    send(socket, C2SMessage::Ping(7, true, vec![])).await;
    let expected: Vec<u8> = S2CMessage::Ping(owner, 7, true, vec![]).into();
    assert_eq!(receive(socket).await, expected);
}

async fn wait_connections(state: &AppState, owner: Uuid, count: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.session.count(owner) != count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Session cleanup timeout");
}

/// Runs in the same isolated store as B0: no global environment races between tests.
pub async fn legacy_lifecycle(
    base: &str,
    client: &reqwest::Client,
    state: &AppState,
    resource: &[u8],
    owner: Uuid,
) {
    assert_eq!(
        client
            .put(format!("{base}/api/avatar"))
            .header("token", "baseline-token")
            .body(resource.to_vec())
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    state.config.write().await.limitations.can_upload = true;
    let observer = Uuid::from_u128(42);
    for (id, token) in [(owner, "second-token"), (observer, "observer-token")] {
        state
            .user_manager
            .insert(
                id,
                token.into(),
                Userinfo {
                    uuid: id,
                    token: Some(token.into()),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    for token in ["baseline-token", "second-token"] {
        assert_eq!(
            client
                .get(format!("{base}/api/"))
                .header("token", token)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(
        client
            .put(format!("{base}/api/avatar"))
            .header("token", "invalid")
            .body(resource.to_vec())
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .put(format!("{base}/api/avatar"))
            .header("token", "second-token")
            .body(vec![0; 1025])
            .send()
            .await
            .unwrap()
            .status(),
        413
    );
    let uploaded = client
        .put(format!("{base}/api/avatar"))
        .header("token", "second-token")
        .body(resource.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(uploaded.status(), 200);
    assert_eq!(uploaded.text().await.unwrap(), "ok");
    let downloaded = client
        .get(format!("{base}/api/{owner}/avatar"))
        .header("token", "observer-token")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        downloaded.as_ref(),
        resource,
        "Small upload must be fully flushed"
    );

    let mut first = connect(base, "baseline-token").await;
    let mut second = connect(base, "second-token").await;
    let mut watcher = connect(base, "observer-token").await;
    wait_connections(state, owner, 2).await;
    send(&mut watcher, C2SMessage::Sub(owner)).await;
    send(&mut watcher, C2SMessage::Sub(owner)).await;
    barrier(&mut watcher, observer).await;
    let arguments = vec![0, 255, 128, 127];
    send(
        &mut first,
        C2SMessage::Ping(u32::MAX, true, arguments.clone()),
    )
    .await;
    let expected: Vec<u8> = S2CMessage::Ping(owner, u32::MAX, true, arguments).into();
    assert_eq!(receive(&mut first).await, expected);
    assert_eq!(receive(&mut watcher).await, expected);

    let equipped = client
        .post(format!("{base}/api/equip"))
        .header("token", "second-token")
        .header("content-type", "application/json")
        .body("[]")
        .send()
        .await
        .unwrap();
    assert_eq!(equipped.status(), 200);
    assert_eq!(equipped.text().await.unwrap(), "ok");
    let changed: Vec<u8> = S2CMessage::Event(owner).into();
    assert_eq!(receive(&mut first).await, changed);
    assert_eq!(receive(&mut second).await, changed);
    assert_eq!(
        receive(&mut watcher).await,
        changed,
        "Repeated subscription must not duplicate events"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), watcher.next())
            .await
            .is_err()
    );

    send(&mut watcher, C2SMessage::Unsub(owner)).await;
    barrier(&mut watcher, observer).await;
    send(&mut first, C2SMessage::Ping(123, true, vec![])).await;
    receive(&mut first).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), watcher.next())
            .await
            .is_err()
    );
    send(&mut watcher, C2SMessage::Sub(owner)).await;
    barrier(&mut watcher, observer).await;

    first.close(None).await.unwrap();
    wait_connections(state, owner, 1).await;
    let reconnected = connect(base, "baseline-token").await;
    wait_connections(state, owner, 2).await;
    drop(reconnected);
    wait_connections(state, owner, 1).await;
    barrier(&mut second, owner).await;
    let ping_barrier: Vec<u8> = S2CMessage::Ping(owner, 7, true, vec![]).into();
    assert_eq!(receive(&mut watcher).await, ping_barrier);
    for token in ["baseline-token", "second-token"] {
        assert_eq!(
            client
                .get(format!("{base}/api/"))
                .header("token", token)
                .send()
                .await
                .unwrap()
                .status(),
            200,
            "Closing old transport must not revoke another token or prevent reconnect"
        );
    }
    let deleted = client
        .delete(format!("{base}/api/avatar"))
        .header("token", "second-token")
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 200);
    assert_eq!(deleted.text().await.unwrap(), "ok");
    assert_eq!(receive(&mut second).await, changed);
    assert_eq!(receive(&mut watcher).await, changed);
    assert_eq!(
        client
            .get(format!("{base}/api/{owner}/avatar"))
            .header("token", "observer-token")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let profile: serde_json::Value = client
        .get(format!("{base}/api/{owner}"))
        .header("token", "observer-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(profile["equipped"], serde_json::json!([]));
    second.close(None).await.unwrap();
    watcher.close(None).await.unwrap();
    wait_connections(state, owner, 0).await;
    wait_connections(state, observer, 0).await;
}
