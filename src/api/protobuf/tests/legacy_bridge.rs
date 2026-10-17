use super::*;
use crate::service::ysm::legacy;
use envelope::Body;

fn golden(name: &str) -> Vec<u8> {
    let hex = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/protocol/m7-vectors.tsv"
    ))
    .lines()
    .filter_map(|line| line.split_once('\t'))
    .find(|(key, _)| *key == name)
    .unwrap()
    .1;
    let mut bytes = vec![0; hex.len() / 2];
    faster_hex::hex_decode(hex.as_bytes(), &mut bytes).unwrap();
    bytes
}

async fn send_frame(socket: &mut Socket, owner: Uuid, message_id: u64, frame: &legacy::Frame) {
    for packet in legacy::fragment(message_id, &legacy::encode(frame).unwrap()).unwrap() {
        old_send(
            socket,
            C2SMessage::Ping(legacy::PING_ID, true, packet.clone()),
        )
        .await;
        let expected: Vec<u8> = S2CMessage::Ping(owner, legacy::PING_ID, true, packet).into();
        assert_eq!(
            bytes(socket).await,
            expected,
            "legacy echo must preserve the original fragment bytes"
        );
    }
}

async fn connect_legacy(base: &str, token: &str, ysm: bool) -> Socket {
    let mut request = (base.replacen("http://", "ws://", 1) + "/ws")
        .into_client_request()
        .unwrap();
    if ysm {
        request
            .headers_mut()
            .insert("x-omnimorph-models", "ysm".parse().unwrap());
    }
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    old_send(&mut socket, C2SMessage::Token(token.as_bytes().to_vec())).await;
    assert_eq!(bytes(&mut socket).await, [0]);
    socket
}

async fn receive_frame(socket: &mut Socket, owner: Uuid, revision: u64) -> legacy::Frame {
    let mut reassembly = legacy::Reassembly::default();
    loop {
        let raw = bytes(socket).await;
        let S2CMessage::Ping(authenticated, function, echo, packet) =
            S2CMessage::try_from(raw.as_slice()).unwrap()
        else {
            panic!("expected M7 Ping");
        };
        assert_eq!(authenticated, owner);
        assert_eq!(function, legacy::PING_ID);
        assert!(!echo);
        if let Some(raw) = reassembly.accept(owner, &packet).unwrap() {
            return legacy::decode(&raw, revision).unwrap();
        }
    }
}

pub(super) async fn integration(base: &str, http: &reqwest::Client, state: &AppState) {
    let owner = Uuid::from_u128(601);
    let observer = Uuid::from_u128(602);
    for (uuid, token) in [(owner, "m7-source"), (observer, "m7-viewer")] {
        state
            .user_manager
            .insert(
                uuid,
                token.into(),
                Userinfo {
                    uuid,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let (raw, content) = crate::service::package::tests::fixture(false, false);
    assert_eq!(
        http.put(format!("{base}/api/avatar"))
            .header("token", "m7-source")
            .body(raw)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let avatar = state.storage.equipped(owner).unwrap().primary.unwrap();
    assert!(
        avatar.schema_sha256.is_empty(),
        "legacy upload has no runtime inventory header"
    );
    let (mut source, _) =
        tokio_tungstenite::connect_async(base.replacen("http://", "ws://", 1) + "/ws")
            .await
            .unwrap();
    old_send(&mut source, C2SMessage::Token(b"m7-source".to_vec())).await;
    assert_eq!(bytes(&mut source).await, [0]);
    let mut viewer = Client::login(base, http, "m7-viewer", 1).await;
    let mut watching = viewer
        .websocket_with_capabilities(vec![
            Capability::YsmPlain as i32,
            Capability::YsmRecovery as i32,
        ])
        .await;
    watching
        .send(WsMessage::Binary(
            viewer
                .message(Body::Subscribe(Subscribe {
                    owners: vec![owner.to_string()],
                    cursors: vec![],
                }))
                .encode_to_vec(),
        ))
        .await
        .unwrap();
    assert!(matches!(receive_proto(&mut watching).await, Body::Ack(_)));
    error(
        receive_proto(&mut watching).await,
        ErrorCode::SnapshotRequired,
    );
    let mut frame = legacy::decode(&golden("snapshot"), avatar.revision).unwrap();
    frame.identity.owner = owner.to_string();
    frame.identity.publisher_session = Uuid::from_u128(603).to_string();
    frame.identity.content_sha256 = content.clone();
    send_frame(&mut source, owner, 100, &frame).await;
    let Body::YsmSnapshot(snapshot) = receive_proto(&mut watching).await else {
        panic!("missing bridged snapshot");
    };
    assert_eq!(snapshot.state.as_ref(), Some(&frame.state));
    assert_eq!(snapshot.identity.as_ref().unwrap().sequence, 0);
    assert_eq!(snapshot.identity.as_ref().unwrap().event_watermark, 2);
    let mut delta = legacy::decode(&golden("delta"), avatar.revision).unwrap();
    delta.identity.owner = owner.to_string();
    delta.identity.publisher_session = frame.identity.publisher_session.clone();
    delta.identity.content_sha256 = content.clone();
    send_frame(&mut source, owner, 101, &delta).await;
    let Body::YsmDelta(changed) = receive_proto(&mut watching).await else {
        panic!("missing bridged delta");
    };
    assert_eq!(changed.identity.unwrap().sequence, 1);
    assert!(changed.state.unwrap().controls.unwrap().values.is_empty());
    let mut event = legacy::decode(&golden("event_delta"), avatar.revision).unwrap();
    event.identity.owner = owner.to_string();
    event.identity.publisher_session = frame.identity.publisher_session;
    event.identity.content_sha256 = content;
    send_frame(&mut source, owner, 102, &event).await;
    let Body::YsmDelta(changed) = receive_proto(&mut watching).await else {
        panic!("missing event's state delta");
    };
    assert_eq!(changed.identity.unwrap().sequence, 2);
    let Body::YsmEvent(published) = receive_proto(&mut watching).await else {
        panic!("missing bridged instantaneous event");
    };
    assert_eq!(published.identity.unwrap().sequence, 3);
    assert_eq!(published.events, event.events);
    send_frame(&mut source, owner, 103, &event).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), watching.next())
            .await
            .is_err()
    );
    source.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.session.count(owner) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let Body::YsmSnapshot(offline) = viewer
        .rpc(Body::SnapshotRequest(SnapshotRequest {
            owner: owner.to_string(),
            ..Default::default()
        }))
        .await
    else {
        panic!("offline M7 state missing");
    };
    assert_eq!(offline.identity.as_ref().unwrap().event_watermark, 3);
    watching.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.session.count(observer) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    reverse_direction(base, http, state, owner, avatar.revision, offline).await;
}

async fn reverse_direction(
    base: &str,
    http: &reqwest::Client,
    state: &AppState,
    owner: Uuid,
    revision: u64,
    mut snapshot: YsmSnapshot,
) {
    let observer = Uuid::from_u128(605);
    let upstream = Uuid::from_u128(606);
    for (uuid, token) in [(observer, "m7-capable"), (upstream, "upstream-figura")] {
        state
            .user_manager
            .insert(
                uuid,
                token.into(),
                Userinfo {
                    uuid,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let mut publisher = Client::login(base, http, "m7-source", 10).await;
    let id = snapshot.identity.as_mut().unwrap();
    id.publisher_session = Uuid::from_u128(604).to_string();
    id.generation = 1;
    id.sequence = 0;
    id.base_sequence = -1;
    id.owner_tick = 100;
    id.event_watermark = 0;
    assert!(matches!(
        publisher.rpc(Body::YsmSnapshot(snapshot.clone())).await,
        Body::Ack(_)
    ));
    let mut capable = connect_legacy(base, "m7-capable", true).await;
    old_send(&mut capable, C2SMessage::Sub(owner)).await;
    let baseline = receive_frame(&mut capable, owner, revision).await;
    assert!(baseline.snapshot);
    assert_eq!(baseline.identity, snapshot.identity.clone().unwrap());
    assert_eq!(baseline.state, snapshot.state.unwrap());
    assert!(baseline.events.is_empty());
    let mut plain = connect_legacy(base, "upstream-figura", false).await;
    old_send(&mut plain, C2SMessage::Sub(owner)).await;
    old_send(&mut plain, C2SMessage::Ping(7, true, vec![])).await;
    let barrier: Vec<u8> = S2CMessage::Ping(upstream, 7, true, vec![]).into();
    assert_eq!(bytes(&mut plain).await, barrier);
    let mut id = baseline.identity;
    id.sequence = 1;
    id.base_sequence = 0;
    id.owner_tick = 101;
    assert!(matches!(
        publisher
            .rpc(Body::YsmDelta(YsmDelta {
                identity: Some(id.clone()),
                state: Some(YsmState {
                    texture: Some(2),
                    ..Default::default()
                })
            }))
            .await,
        Body::Ack(_)
    ));
    let changed = receive_frame(&mut capable, owner, revision).await;
    assert!(!changed.snapshot);
    assert_eq!(changed.identity.sequence, 1);
    assert_eq!(changed.state.texture, Some(2));
    id.sequence = 2;
    id.base_sequence = 1;
    id.owner_tick = 102;
    id.event_watermark = 1;
    let event = Body::YsmEvent(YsmEvent {
        identity: Some(id),
        events: vec![Event {
            sequence: 1,
            kind: EventKind::Sync as i32,
            arguments: vec![7.0],
        }],
    });
    assert!(matches!(publisher.rpc(event.clone()).await, Body::Ack(_)));
    let delivered = receive_frame(&mut capable, owner, revision).await;
    assert_eq!(delivered.identity.sequence, 2);
    assert_eq!(delivered.events.len(), 1);
    assert_eq!(delivered.events[0].arguments, vec![7.0]);
    assert!(matches!(publisher.rpc(event).await, Body::Ack(_)));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), capable.next())
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), plain.next())
            .await
            .is_err(),
        "upstream Figura received YSM traffic"
    );
    assert!(matches!(
        publisher
            .rpc(Body::Delete(Delete {
                operation_id: resource::new_operation_id(),
                asset_id: "avatar".into(),
                expected_revision: revision
            }))
            .await,
        Body::Ack(_)
    ));
    let changed: Vec<u8> = S2CMessage::Event(owner).into();
    assert_eq!(bytes(&mut capable).await, changed);
    assert_eq!(bytes(&mut plain).await, changed);
    capable.close(None).await.unwrap();
    plain.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.session.count(observer) != 0 || state.session.count(upstream) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
