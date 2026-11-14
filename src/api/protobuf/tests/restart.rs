use super::*;
use crate::auth::UManager;
use axum::Router;
use envelope::Body;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::RwLock;

pub(crate) struct Restart {
    viewer: Client,
    owner: Uuid,
    observer: Uuid,
    cursor: Resume,
}

pub(crate) async fn prepare(base: &str, http: &reqwest::Client, state: &AppState) -> Restart {
    let owner = Uuid::from_u128(701);
    let observer = Uuid::from_u128(702);
    for (uuid, token) in [(owner, "restart-owner"), (observer, "restart-viewer")] {
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
    let mut publisher = Client::login(base, http, "restart-owner", 1).await;
    let viewer = Client::login(base, http, "restart-viewer", 1).await;
    let (raw, content) = crate::service::package::tests::fixture(false, false);
    let avatar = upload_resource(
        &mut publisher,
        AvatarDescriptor {
            asset_id: "restart".into(),
            model_type: 2,
            distribution_format: 2,
            raw_sha256: crate::storage::sha256(&raw),
            raw_length: raw.len() as u64,
            content_sha256: content.clone(),
            schema_sha256: vec![5; 32],
            ..Default::default()
        },
        &raw,
    )
    .await;
    assert!(matches!(
        publisher
            .rpc(Body::Equip(Equip {
                operation_id: resource::new_operation_id(),
                asset_id: avatar.asset_id,
                asset_revision: avatar.revision,
                ..Default::default()
            }))
            .await,
        Body::Equipped(_)
    ));
    let mut id = YsmIdentity {
        owner: owner.to_string(),
        asset_revision: avatar.revision,
        publisher_session: Uuid::from_u128(703).to_string(),
        generation: 1,
        base_sequence: -1,
        content_sha256: content,
        schema_sha256: vec![5; 32],
        owner_tick: 100,
        ..Default::default()
    };
    assert!(matches!(
        publisher
            .rpc(Body::YsmSnapshot(YsmSnapshot {
                identity: Some(id.clone()),
                state: Some(YsmState {
                    texture: Some(0),
                    controls: Some(Controls::default()),
                    actions: Some(Actions::default()),
                    controllers: Some(Controllers::default()),
                    playbacks: Some(Playbacks::default()),
                    display: None,
                })
            }))
            .await,
        Body::Ack(_)
    ));
    let cursor = Resume {
        owner: owner.to_string(),
        server_epoch: state.protocol_sessions.server_epoch,
        revision: avatar.revision,
        publisher_session: id.publisher_session.clone(),
        generation: 1,
        sequence: 0,
        event_watermark: 0,
    };
    id.sequence = 1;
    id.base_sequence = 0;
    id.owner_tick = 101;
    assert!(matches!(
        publisher
            .rpc(Body::YsmDelta(YsmDelta {
                identity: Some(id.clone()),
                state: Some(YsmState {
                    texture: Some(1),
                    ..Default::default()
                })
            }))
            .await,
        Body::Ack(_)
    ));
    id.sequence = 2;
    id.base_sequence = 1;
    id.owner_tick = 102;
    id.event_watermark = 1;
    assert!(matches!(
        publisher
            .rpc(Body::YsmEvent(YsmEvent {
                identity: Some(id),
                events: vec![Event {
                    sequence: 1,
                    kind: EventKind::Sync as i32,
                    arguments: vec![1.0]
                }]
            }))
            .await,
        Body::Ack(_)
    ));
    Restart {
        viewer,
        owner,
        observer,
        cursor,
    }
}

impl Restart {
    pub(crate) async fn verify(mut self, config: Arc<RwLock<crate::state::Config>>, root: PathBuf) {
        // The old listener has shut down. Open the persisted database and create
        // fresh process state: no old session registry, tokens, queues or hub.
        let storage = Arc::new(crate::storage::Storage::open(root).unwrap());
        let sessions = Arc::new(crate::service::session::ProtocolSessions::new(
            storage.instance_id(),
        ));
        assert_ne!(sessions.server_epoch, self.cursor.server_epoch);
        let users = UManager::open(storage.clone()).unwrap();
        assert!(users.get_by_uuid(&self.owner).is_some(),
            "an observer must resolve the equipped owner's UUID after restart without owner re-login");
        // Synthetic identity verification for this new server, independent of
        // its revoked scoped token. No real account or external auth is used.
        users
            .insert(
                self.observer,
                "restart-viewer".into(),
                Userinfo {
                    uuid: self.observer,
                    ..Default::default()
                },
            )
            .unwrap();
        let state = AppState {
            uptime: tokio::time::Instant::now(),
            user_manager: Arc::new(users),
            session: Arc::new(crate::service::session::SessionRegistry::default()),
            subscribes: Arc::new(crate::service::event::EventHub::default()),
            storage,
            protocol_sessions: sessions,
            transfers: Arc::new(crate::service::resource::Transfers::default()),
            figura_versions: Arc::new(RwLock::new(None)),
            config,
        };
        let router = Router::new()
            .merge(super::super::router(true))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        self.viewer.base = base.clone();
        error(
            self.viewer.rpc(Body::Heartbeat(Heartbeat::default())).await,
            ErrorCode::Unauthenticated,
        );
        let mut viewer = Client::login(&base, &self.viewer.http, "restart-viewer", 2).await;
        assert_eq!(viewer.scope.instance_id, self.viewer.scope.instance_id);
        let mut socket = viewer
            .websocket_with_capabilities(vec![
                Capability::YsmPlain as i32,
                Capability::YsmRecovery as i32,
            ])
            .await;
        socket
            .send(WsMessage::Binary(
                viewer
                    .message(Body::Subscribe(Subscribe {
                        owners: vec![self.owner.to_string()],
                        cursors: vec![self.cursor],
                    }))
                    .encode_to_vec(),
            ))
            .await
            .unwrap();
        assert!(matches!(receive_proto(&mut socket).await, Body::Ack(_)));
        error(receive_proto(&mut socket).await, ErrorCode::CursorExpired);
        let Body::YsmSnapshot(snapshot) = receive_proto(&mut socket).await else {
            panic!("restarted server lost its baseline");
        };
        let id = snapshot.identity.unwrap();
        assert_eq!(
            (id.sequence, id.owner_tick, id.event_watermark),
            (2, 102, 1)
        );
        assert_eq!(snapshot.state.unwrap().texture, Some(1));
        assert_eq!(
            state.session.count(self.owner),
            0,
            "offline owner was recreated as a live publisher"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), socket.next())
                .await
                .is_err(),
            "restart replayed a historical instantaneous event"
        );
        socket.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.session.count(self.observer) != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        stop.send(()).unwrap();
        server.await.unwrap();
    }
}
