use super::*;
use envelope::Body;

fn capabilities() -> Vec<i32> {
    vec![
        Capability::Figura as i32,
        Capability::YsmPlain as i32,
        Capability::YsmRecovery as i32,
    ]
}

async fn command(client: &mut Client, socket: &mut Socket, body: Body) -> Body {
    socket
        .send(WsMessage::Binary(client.message(body).encode_to_vec()))
        .await
        .unwrap();
    receive_proto(socket).await
}

pub(super) async fn integration(base: &str, http: &reqwest::Client, state: &AppState) {
    let owner = Uuid::from_u128(501);
    let observer = Uuid::from_u128(502);
    let figura_peer = Uuid::from_u128(503);
    for (owner, token) in [
        (owner, "recovery-owner"),
        (observer, "recovery-observer"),
        (figura_peer, "recovery-figura"),
    ] {
        state
            .user_manager
            .insert(
                owner,
                token.into(),
                Userinfo {
                    uuid: owner,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let mut host = Client::login(base, http, "recovery-owner", 1).await;
    let mut viewer = Client::login(base, http, "recovery-observer", 1).await;
    let mut figura = Client::login(base, http, "recovery-figura", 1).await;
    let (raw, content_hash) = crate::service::package::tests::fixture(false, false);
    let avatar = upload_resource(
        &mut host,
        AvatarDescriptor {
            asset_id: "recovery".into(),
            model_type: 2,
            distribution_format: 2,
            raw_sha256: crate::storage::sha256(&raw),
            raw_length: raw.len() as u64,
            content_sha256: content_hash,
            schema_sha256: vec![3; 32],
            ..Default::default()
        },
        &raw,
    )
    .await;
    assert!(matches!(
        host.rpc(Body::Equip(Equip {
            operation_id: resource::new_operation_id(),
            asset_id: avatar.asset_id.clone(),
            asset_revision: avatar.revision,
            ..Default::default()
        }))
        .await,
        Body::Equipped(_)
    ));
    let mut publisher = host.websocket_with_capabilities(capabilities()).await;
    let first = YsmSnapshot {
        identity: Some(YsmIdentity {
            owner: owner.to_string(),
            publisher_session: Uuid::from_u128(504).to_string(),
            asset_revision: avatar.revision,
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
    let mut entity = first.clone();
    entity.identity.as_mut().unwrap().owner = format!("entity:{owner}");
    error(
        host.rpc(Body::YsmSnapshot(entity)).await,
        ErrorCode::AuthorityUnavailable,
    );
    error(
        viewer
            .rpc(Body::SnapshotRequest(SnapshotRequest {
                owner: format!("entity:{owner}"),
                ..Default::default()
            }))
            .await,
        ErrorCode::AuthorityUnavailable,
    );
    let mut early = viewer.websocket_with_capabilities(capabilities()).await;
    assert!(matches!(
        command(
            &mut viewer,
            &mut early,
            Body::Subscribe(Subscribe {
                owners: vec![owner.to_string()],
                cursors: vec![],
            })
        )
        .await,
        Body::Ack(_)
    ));
    error(receive_proto(&mut early).await, ErrorCode::SnapshotRequired);
    assert!(
        matches!(receive_proto(&mut publisher).await, Body::SnapshotRequest(request) if request.owner == owner.to_string())
    );
    assert!(
        matches!(command(&mut host, &mut publisher, Body::YsmSnapshot(first.clone())).await, Body::Ack(ack) if ack.sequence == 0)
    );
    assert_eq!(
        receive_proto(&mut early).await,
        Body::YsmSnapshot(first.clone())
    );
    early.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.session.count(observer) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    viewer = Client::login(base, http, "recovery-observer", 2).await;
    let mut watching = viewer.websocket_with_capabilities(capabilities()).await;
    assert!(matches!(
        command(
            &mut viewer,
            &mut watching,
            Body::Subscribe(Subscribe {
                owners: vec![owner.to_string()],
                cursors: vec![]
            })
        )
        .await,
        Body::Ack(_)
    ));
    assert_eq!(
        receive_proto(&mut watching).await,
        Body::YsmSnapshot(first.clone())
    );
    let mut plain_peer = figura.websocket().await;
    assert!(matches!(
        command(
            &mut figura,
            &mut plain_peer,
            Body::Subscribe(Subscribe {
                owners: vec![owner.to_string()],
                cursors: vec![]
            })
        )
        .await,
        Body::Ack(_)
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), plain_peer.next())
            .await
            .is_err()
    );

    let mut id = first.identity.clone().unwrap();
    id.sequence = 1;
    id.base_sequence = 0;
    id.owner_tick = 1;
    let delta = Body::YsmDelta(YsmDelta {
        identity: Some(id.clone()),
        state: Some(YsmState {
            texture: Some(0),
            ..Default::default()
        }),
    });
    assert!(
        matches!(command(&mut host, &mut publisher, delta.clone()).await, Body::Ack(ack) if ack.sequence == 1)
    );
    assert_eq!(receive_proto(&mut watching).await, delta);
    id.sequence = 2;
    id.base_sequence = 1;
    id.owner_tick = 2;
    id.event_watermark = 1;
    let event = Body::YsmEvent(YsmEvent {
        identity: Some(id.clone()),
        events: vec![Event {
            sequence: 1,
            kind: EventKind::Sync as i32,
            arguments: vec![9.0],
        }],
    });
    assert!(
        matches!(command(&mut host, &mut publisher, event.clone()).await, Body::Ack(ack) if ack.sequence == 2)
    );
    assert_eq!(receive_proto(&mut watching).await, event);
    assert!(
        matches!(command(&mut host, &mut publisher, event).await, Body::Ack(ack) if ack.sequence == 2)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), watching.next())
            .await
            .is_err()
    );
    watching.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.session.count(observer) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    id.sequence = 3;
    id.base_sequence = 2;
    id.owner_tick = 3;
    let next = Body::YsmDelta(YsmDelta {
        identity: Some(id.clone()),
        state: Some(YsmState {
            texture: Some(1),
            ..Default::default()
        }),
    });
    assert!(
        matches!(command(&mut host, &mut publisher, next.clone()).await, Body::Ack(ack) if ack.sequence == 3)
    );
    viewer = Client::login(base, http, "recovery-observer", 3).await;
    let mut resumed = viewer.websocket_with_capabilities(capabilities()).await;
    let cursor = Resume {
        owner: owner.to_string(),
        server_epoch: state.protocol_sessions.server_epoch,
        revision: avatar.revision,
        publisher_session: id.publisher_session.clone(),
        generation: 1,
        sequence: 2,
        event_watermark: 1,
    };
    assert!(matches!(
        command(&mut viewer, &mut resumed, Body::Resume(cursor.clone())).await,
        Body::Ack(_)
    ));
    assert_eq!(receive_proto(&mut resumed).await, next);
    let mut expired = cursor;
    expired.server_epoch ^= 1;
    assert!(matches!(
        command(&mut viewer, &mut resumed, Body::Resume(expired)).await,
        Body::Ack(_)
    ));
    error(receive_proto(&mut resumed).await, ErrorCode::CursorExpired);
    let Body::YsmSnapshot(recovered) = receive_proto(&mut resumed).await else {
        panic!("missing recovered snapshot");
    };
    assert_eq!(recovered.identity.unwrap().event_watermark, 1);
    assert_eq!(recovered.state.unwrap().texture, Some(1));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), resumed.next())
            .await
            .is_err(),
        "historical events were replayed after baseline recovery"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), plain_peer.next())
            .await
            .is_err()
    );
    assert!(matches!(
        host.rpc(Body::Delete(Delete {
            operation_id: resource::new_operation_id(),
            asset_id: avatar.asset_id,
            expected_revision: avatar.revision
        }))
        .await,
        Body::Ack(_)
    ));
    for socket in [&mut publisher, &mut resumed, &mut plain_peer] {
        assert!(
            matches!(receive_proto(socket).await, Body::AvatarChanged(change) if change.tombstone)
        );
    }
    id.sequence = 4;
    id.base_sequence = 3;
    error(
        command(
            &mut host,
            &mut publisher,
            Body::YsmDelta(YsmDelta {
                identity: Some(id),
                state: Some(YsmState::default()),
            }),
        )
        .await,
        ErrorCode::NotFound,
    );
    error(
        viewer
            .rpc(Body::SnapshotRequest(SnapshotRequest {
                owner: owner.to_string(),
                ..Default::default()
            }))
            .await,
        ErrorCode::SnapshotRequired,
    );
    for socket in [&mut publisher, &mut resumed, &mut plain_peer] {
        socket.close(None).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while [owner, observer, figura_peer]
            .into_iter()
            .any(|owner| state.session.count(owner) != 0)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
