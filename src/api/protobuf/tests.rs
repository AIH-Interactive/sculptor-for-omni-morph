use super::*;
use crate::{
    api::figura::websocket::{C2SMessage, S2CMessage},
    auth::Userinfo,
    service::resource,
};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message as WsMessage},
    MaybeTlsStream, WebSocketStream,
};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Client {
    base: String,
    http: reqwest::Client,
    scope: AuthResult,
    epoch: u64,
    sequence: u64,
}

impl Client {
    async fn login(base: &str, http: &reqwest::Client, token: &str, epoch: u64) -> Self {
        let capabilities = http
            .get(format!("{base}/api/protobuf/v1/capabilities"))
            .send()
            .await
            .unwrap();
        assert_eq!(capabilities.headers()["content-type"], CONTENT_TYPE);
        let capabilities = Envelope::decode(capabilities.bytes().await.unwrap()).unwrap();
        let Some(envelope::Body::HelloAck(capabilities)) = capabilities.body else {
            panic!("capabilities missing");
        };
        assert_eq!(
            capabilities.capabilities,
            vec![
                Capability::Figura as i32,
                Capability::YsmPlain as i32,
                Capability::YsmEncrypted as i32
            ]
        );
        let request = Envelope {
            major: 1,
            request_id: 1,
            connection_epoch: epoch,
            body: Some(envelope::Body::Auth(Auth {
                token: token.into(),
                instance_id: capabilities.instance_id,
                ..Default::default()
            })),
            ..Default::default()
        };
        let result = http
            .post(format!("{base}/api/protobuf/v1/rpc"))
            .header("content-type", CONTENT_TYPE)
            .body(request.encode_to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), 200);
        let result = Envelope::decode(result.bytes().await.unwrap()).unwrap();
        let Some(envelope::Body::AuthResult(scope)) = result.body else {
            panic!("authentication failed");
        };
        assert_eq!(scope.protocol_major, 1);
        assert!(scope.expires_unix_seconds > crate::storage::now_seconds());
        Self {
            base: base.into(),
            http: http.clone(),
            scope,
            epoch,
            sequence: 1,
        }
    }

    fn message(&mut self, body: envelope::Body) -> Envelope {
        self.sequence += 1;
        Envelope {
            major: 1,
            request_id: self.sequence,
            session_id: self.scope.session_id.clone(),
            connection_epoch: self.epoch,
            body: Some(body),
        }
    }

    async fn rpc(&mut self, body: envelope::Body) -> envelope::Body {
        let request = self.message(body);
        let response = self
            .http
            .post(format!("{}/api/protobuf/v1/rpc", self.base))
            .header("content-type", CONTENT_TYPE)
            .bearer_auth(&self.scope.session_id)
            .body(request.encode_to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.headers()["content-type"], CONTENT_TYPE);
        let response = Envelope::decode(response.bytes().await.unwrap()).unwrap();
        assert_eq!(response.request_id, request.request_id);
        response.body.unwrap()
    }

    async fn websocket(&mut self) -> Socket {
        let mut request = (self.base.replacen("http://", "ws://", 1) + "/ws/protobuf/v1")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("sec-websocket-protocol", SUBPROTOCOL.parse().unwrap());
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", self.scope.session_id).parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("x-omnimorph-epoch", self.epoch.to_string().parse().unwrap());
        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(response.headers()["sec-websocket-protocol"], SUBPROTOCOL);
        let hello = self.message(envelope::Body::Hello(Hello {
            supported_majors: vec![1],
            client_version: "network-test".into(),
            capabilities: vec![Capability::Figura as i32],
        }));
        socket
            .send(WsMessage::Binary(hello.encode_to_vec()))
            .await
            .unwrap();
        assert!(matches!(
            receive_proto(&mut socket).await,
            envelope::Body::HelloAck(_)
        ));
        socket
    }

    async fn file(&self, path: &str) -> reqwest::Response {
        self.http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.scope.session_id)
            .header("x-omnimorph-epoch", self.epoch)
            .send()
            .await
            .unwrap()
    }
}

async fn bytes(socket: &mut Socket) -> Vec<u8> {
    let message = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .expect("socket response timeout")
        .expect("socket closed")
        .unwrap();
    assert!(message.is_binary());
    message.into_data()
}

async fn receive_proto(socket: &mut Socket) -> envelope::Body {
    Envelope::decode(bytes(socket).await.as_slice())
        .unwrap()
        .body
        .unwrap()
}

async fn old_send(socket: &mut Socket, message: C2SMessage) {
    let encoded: Vec<u8> = message.into();
    socket.send(WsMessage::Binary(encoded)).await.unwrap();
}

fn error(body: envelope::Body, expected: ErrorCode) {
    assert!(matches!(body, envelope::Body::Error(error) if error.code == expected as i32));
}

pub async fn integration(base: &str, http: &reqwest::Client, state: &AppState, raw: &[u8]) {
    use envelope::Body;
    let owner = Uuid::from_u128(301);
    let watcher = Uuid::from_u128(302);
    for (id, token) in [(owner, "protobuf-owner"), (watcher, "legacy-watcher")] {
        state
            .user_manager
            .insert(
                id,
                token.into(),
                Userinfo {
                    uuid: id,
                    token: Some(token.into()),
                    rank: "tester".into(),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    *state.figura_versions.write().await = Some(crate::FiguraVersions {
        release: "0.1.5".into(),
        prerelease: "0.1.6".into(),
    });
    let mut client = Client::login(base, http, "protobuf-owner", 1).await;
    let mut socket = client.websocket().await;
    let (mut old, _) =
        tokio_tungstenite::connect_async(base.replacen("http://", "ws://", 1) + "/ws")
            .await
            .unwrap();
    old_send(&mut old, C2SMessage::Token(b"legacy-watcher".to_vec())).await;
    assert_eq!(bytes(&mut old).await, vec![0]);
    old_send(&mut old, C2SMessage::Sub(owner)).await;
    old_send(&mut old, C2SMessage::Ping(7, true, vec![])).await;
    bytes(&mut old).await;
    let subscribe = client.message(Body::Subscribe(Subscribe {
        owners: vec![watcher.to_string(), watcher.to_string()],
        ..Default::default()
    }));
    socket
        .send(WsMessage::Binary(subscribe.encode_to_vec()))
        .await
        .unwrap();
    assert!(matches!(receive_proto(&mut socket).await, Body::Ack(_)));

    let arguments = vec![0, 255, 128, 127];
    let ping = client.message(Body::FiguraPing(FiguraPing {
        function_id: -1,
        echo: true,
        arguments: arguments.clone(),
        ..Default::default()
    }));
    socket
        .send(WsMessage::Binary(ping.encode_to_vec()))
        .await
        .unwrap();
    assert!(
        matches!(receive_proto(&mut socket).await, Body::FiguraPing(ping) if ping.owner == owner.to_string() && ping.function_id == -1 && ping.arguments == arguments)
    );
    let expected: Vec<u8> = S2CMessage::Ping(owner, u32::MAX, true, arguments.clone()).into();
    assert_eq!(bytes(&mut old).await, expected);
    old_send(&mut old, C2SMessage::Ping(123, true, arguments.clone())).await;
    let expected: Vec<u8> = S2CMessage::Ping(watcher, 123, true, arguments.clone()).into();
    assert_eq!(bytes(&mut old).await, expected);
    assert!(
        matches!(receive_proto(&mut socket).await, Body::FiguraPing(ping) if ping.owner == watcher.to_string() && ping.arguments == arguments)
    );

    assert_eq!(
        http.put(format!("{base}/api/avatar"))
            .header("token", "protobuf-owner")
            .body(raw.to_vec())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let profile = client
        .rpc(Body::ProfileRequest(ProfileRequest {
            owner: owner.to_string(),
        }))
        .await;
    let Body::Profile(profile) = profile else {
        panic!("profile missing");
    };
    assert_eq!(profile.rank, "tester");
    assert_eq!(profile.badges.as_ref().unwrap().special.len(), 6);
    assert_eq!(profile.badges.as_ref().unwrap().pride.len(), 25);
    assert!(profile.can_upload);
    let equipped = profile.equipped.unwrap();
    let avatar = equipped.primary.unwrap();
    assert_eq!(avatar.raw_sha256, crate::storage::sha256(raw));
    let grant = client
        .rpc(Body::DownloadRequest(DownloadRequest {
            owner: owner.to_string(),
            asset_id: avatar.asset_id,
            revision: avatar.revision,
        }))
        .await;
    let Body::TransferGrant(grant) = grant else {
        panic!("download grant missing");
    };
    let downloaded = client.file(&grant.relative_path).await;
    assert_eq!(
        downloaded.headers()["content-type"],
        "application/octet-stream"
    );
    assert_eq!(downloaded.bytes().await.unwrap().as_ref(), raw);
    let info = client.rpc(Body::ServerInfo(ServerInfo::default())).await;
    assert!(
        matches!(info, Body::ServerInfo(info) if info.release == "0.1.5" && info.limits.unwrap().envelope_bytes == 65536)
    );

    let operation = resource::new_operation_id();
    let descriptor = AvatarDescriptor {
        owner: owner.to_string(),
        asset_id: "protobuf-avatar".into(),
        model_type: ModelType::ModelFigura as i32,
        distribution_format: DistributionFormat::FiguraCompressedNbt as i32,
        raw_sha256: crate::storage::sha256(raw),
        raw_length: raw.len() as u64,
        ..Default::default()
    };
    let request = UploadRequest {
        operation_id: operation.clone(),
        resource: Some(descriptor.clone()),
    };
    let grant = client.rpc(Body::UploadRequest(request)).await;
    let Body::TransferGrant(grant) = grant else {
        panic!("upload grant missing");
    };
    error(
        client
            .rpc(Body::Equip(Equip {
                operation_id: resource::new_operation_id(),
                asset_id: descriptor.asset_id.clone(),
                asset_revision: 1,
                expected_revision: equipped.revision,
                ..Default::default()
            }))
            .await,
        ErrorCode::NotFound,
    );
    let uploaded = http
        .put(format!("{base}{}", grant.relative_path))
        .bearer_auth(&client.scope.session_id)
        .header("x-omnimorph-epoch", 1)
        .header("content-type", "application/octet-stream")
        .body(raw.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(uploaded.status(), 204);
    let commit = CommitUpload {
        operation_id: operation,
        raw_sha256: descriptor.raw_sha256.clone(),
        raw_length: descriptor.raw_length,
    };
    let committed = client.rpc(Body::CommitUpload(commit.clone())).await;
    let Body::Avatar(committed) = committed else {
        panic!("commit missing");
    };
    assert_eq!(committed.revision, 1);
    assert_eq!(committed.legacy_figura_hash, avatar.legacy_figura_hash);
    assert_eq!(
        client.rpc(Body::CommitUpload(commit)).await,
        Body::Avatar(committed.clone())
    );
    let equip = Equip {
        operation_id: resource::new_operation_id(),
        asset_id: committed.asset_id.clone(),
        asset_revision: committed.revision,
        expected_revision: equipped.revision,
        ..Default::default()
    };
    let selected = client.rpc(Body::Equip(equip.clone())).await;
    let Body::Equipped(selected) = selected else {
        panic!("equipment missing");
    };
    assert!(
        matches!(receive_proto(&mut socket).await, Body::AvatarChanged(change) if change.equipped.as_ref() == Some(&selected))
    );
    let changed: Vec<u8> = S2CMessage::Event(owner).into();
    assert_eq!(bytes(&mut old).await, changed);
    assert_eq!(
        client.rpc(Body::Equip(equip)).await,
        Body::Equipped(selected.clone())
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.next())
            .await
            .is_err()
    );
    assert!(tokio::time::timeout(Duration::from_millis(100), old.next())
        .await
        .is_err());
    let legacy = http
        .get(format!("{base}/api/{owner}/avatar"))
        .header("token", "legacy-watcher")
        .send()
        .await
        .unwrap();
    assert_eq!(legacy.bytes().await.unwrap().as_ref(), raw);

    let mut forged = descriptor;
    forged.owner = watcher.to_string();
    error(
        client
            .rpc(Body::UploadRequest(UploadRequest {
                operation_id: resource::new_operation_id(),
                resource: Some(forged),
            }))
            .await,
        ErrorCode::Forbidden,
    );
    error(
        client
            .rpc(Body::FiguraPing(FiguraPing {
                owner: watcher.to_string(),
                ..Default::default()
            }))
            .await,
        ErrorCode::Forbidden,
    );
    error(
        client
            .rpc(Body::FiguraPing(FiguraPing {
                arguments: vec![0; 1025],
                ..Default::default()
            }))
            .await,
        ErrorCode::LimitExceeded,
    );
    error(
        client
            .rpc(Body::Equip(Equip {
                operation_id: resource::new_operation_id(),
                asset_id: committed.asset_id.clone(),
                asset_revision: committed.revision,
                expected_revision: 0,
                ..Default::default()
            }))
            .await,
        ErrorCode::Conflict,
    );
    client
        .rpc(Body::Delete(Delete {
            operation_id: resource::new_operation_id(),
            asset_id: committed.asset_id.clone(),
            expected_revision: committed.revision,
        }))
        .await;
    assert!(
        matches!(receive_proto(&mut socket).await, Body::AvatarChanged(change) if change.tombstone && change.equipped.as_ref().is_some_and(|equipped| equipped.primary.is_none()))
    );
    assert_eq!(bytes(&mut old).await, changed);
    assert_eq!(
        http.get(format!("{base}/api/{owner}/avatar"))
            .header("token", "legacy-watcher")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let deleted = client
        .file(&format!(
            "/api/protobuf/v1/resources/{owner}/{}/{}",
            committed.asset_id, committed.revision
        ))
        .await;
    assert_eq!(deleted.status(), 404);
    let invalid = http
        .post(format!("{base}/api/protobuf/v1/rpc"))
        .header("content-type", CONTENT_TYPE)
        .body(
            Envelope {
                major: 99,
                body: Some(Body::Heartbeat(Heartbeat::default())),
                ..Default::default()
            }
            .encode_to_vec(),
        )
        .send()
        .await
        .unwrap();
    error(
        Envelope::decode(invalid.bytes().await.unwrap())
            .unwrap()
            .body
            .unwrap(),
        ErrorCode::UnsupportedVersion,
    );
    let oversized = http
        .post(format!("{base}/api/protobuf/v1/rpc"))
        .header("content-type", CONTENT_TYPE)
        .body(vec![0; wire::MAX_ENVELOPE_BYTES + 1])
        .send()
        .await
        .unwrap();
    error(
        Envelope::decode(oversized.bytes().await.unwrap())
            .unwrap()
            .body
            .unwrap(),
        ErrorCode::LimitExceeded,
    );

    let mut replacement = client.websocket().await;
    socket.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.session.count(owner) != 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        client
            .rpc(Body::ProfileRequest(ProfileRequest {
                owner: owner.to_string()
            }))
            .await,
        Body::Profile(_)
    ));
    let ping = client.message(Body::FiguraPing(FiguraPing {
        echo: true,
        function_id: 5,
        ..Default::default()
    }));
    replacement
        .send(WsMessage::Binary(ping.encode_to_vec()))
        .await
        .unwrap();
    assert!(matches!(
        receive_proto(&mut replacement).await,
        Body::FiguraPing(_)
    ));
    replacement.close(None).await.unwrap();
    old.close(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.session.count(owner) != 0 || state.session.count(watcher) != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    // A fresh HTTP scope is bound to both the bearer and the connection epoch.
    let mut authenticated = Client::login(base, http, "protobuf-owner", 20).await;
    authenticated.epoch = 21;
    error(
        authenticated
            .rpc(Body::Heartbeat(Heartbeat::default()))
            .await,
        ErrorCode::Unauthenticated,
    );
    authenticated.epoch = 20;
    let mut mismatched = authenticated.message(Body::Heartbeat(Heartbeat::default()));
    mismatched.session_id = "forged".into();
    let rejected = http
        .post(format!("{base}/api/protobuf/v1/rpc"))
        .header("content-type", CONTENT_TYPE)
        .bearer_auth(&authenticated.scope.session_id)
        .body(mismatched.encode_to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 401);
    error(
        Envelope::decode(rejected.bytes().await.unwrap())
            .unwrap()
            .body
            .unwrap(),
        ErrorCode::Unauthenticated,
    );
    ysm_resources(base, http, state, raw).await;
    failed_ysm_uploads(base, http, state).await;
    cancelled_legacy_upload(base, state).await;
}

async fn cancelled_legacy_upload(base: &str, state: &AppState) {
    use tokio::io::AsyncWriteExt;
    let owner = Uuid::from_u128(403);
    state
        .user_manager
        .insert(
            owner,
            "cancel-upload".into(),
            Userinfo {
                uuid: owner,
                ..Default::default()
            },
        )
        .unwrap();
    let directory = state
        .storage
        .upload_path(owner, Uuid::nil())
        .parent()
        .unwrap()
        .to_owned();
    let prefix = owner.to_string();
    let has_staging_file = || {
        std::fs::read_dir(&directory).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&prefix)
        })
    };
    let address = base.strip_prefix("http://").unwrap();
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket.write_all(format!("PUT /api/avatar HTTP/1.1\r\nHost: {address}\r\ntoken: cancel-upload\r\nTransfer-Encoding: chunked\r\n\r\n8\r\npending!\r\n").as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !has_staging_file() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut client = Client::login(base, &reqwest::Client::new(), "cancel-upload", 1).await;
    let (raw, content) = crate::service::package::tests::fixture(false, false);
    let descriptor = AvatarDescriptor {
        asset_id: "concurrency".into(),
        model_type: ModelType::ModelYsmPlain as i32,
        distribution_format: DistributionFormat::YsmDistributionPackage as i32,
        raw_sha256: crate::storage::sha256(&raw),
        raw_length: raw.len() as u64,
        content_sha256: content,
        schema_sha256: vec![1; 32],
        ..Default::default()
    };
    assert!(matches!(
        client
            .rpc(envelope::Body::UploadRequest(UploadRequest {
                operation_id: resource::new_operation_id(),
                resource: Some(descriptor.clone()),
            }))
            .await,
        envelope::Body::TransferGrant(_)
    ));
    let blocked = UploadRequest {
        operation_id: resource::new_operation_id(),
        resource: Some(descriptor),
    };
    error(
        client
            .rpc(envelope::Body::UploadRequest(blocked.clone()))
            .await,
        ErrorCode::LimitExceeded,
    );
    drop(socket); // No terminating HTTP chunk: the body was cancelled before publication.
    tokio::time::timeout(Duration::from_secs(2), async {
        while has_staging_file() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(!resource::legacy_path(owner, false).exists());
    assert!(state.storage.equipped(owner).unwrap().primary.is_none());
    assert!(matches!(
        client.rpc(envelope::Body::UploadRequest(blocked)).await,
        envelope::Body::TransferGrant(_)
    ));
}

async fn failed_ysm_uploads(base: &str, http: &reqwest::Client, state: &AppState) {
    use envelope::Body;
    let owner = Uuid::from_u128(402);
    state
        .user_manager
        .insert(
            owner,
            "upload-failures".into(),
            Userinfo {
                uuid: owner,
                ..Default::default()
            },
        )
        .unwrap();
    let mut client = Client::login(base, http, "upload-failures", 1).await;
    let (raw, identity) = crate::service::package::tests::fixture(true, false);
    let descriptor = AvatarDescriptor {
        asset_id: "failed".into(),
        model_type: ModelType::ModelYsmPlain as i32,
        distribution_format: DistributionFormat::YsmDistributionPackage as i32,
        raw_sha256: crate::storage::sha256(&raw),
        raw_length: raw.len() as u64,
        content_sha256: identity,
        schema_sha256: vec![7; 32],
        ..Default::default()
    };
    let operation = resource::new_operation_id();
    let request = UploadRequest {
        operation_id: operation.clone(),
        resource: Some(descriptor.clone()),
    };
    let Body::TransferGrant(grant) = client.rpc(Body::UploadRequest(request.clone())).await else {
        panic!("missing grant");
    };
    let mut conflict = request;
    conflict.resource.as_mut().unwrap().asset_id = "different".into();
    error(
        client.rpc(Body::UploadRequest(conflict)).await,
        ErrorCode::Conflict,
    );
    let mut spare = descriptor.clone();
    spare.asset_id = "spare".into();
    assert!(matches!(
        client
            .rpc(Body::UploadRequest(UploadRequest {
                operation_id: resource::new_operation_id(),
                resource: Some(spare),
            }))
            .await,
        Body::TransferGrant(_)
    ));
    let third = UploadRequest {
        operation_id: resource::new_operation_id(),
        resource: Some(descriptor),
    };
    error(
        client.rpc(Body::UploadRequest(third.clone())).await,
        ErrorCode::LimitExceeded,
    );
    assert_eq!(
        http.put(format!("{base}/api/avatar"))
            .header("token", "upload-failures")
            .body(raw.clone())
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
    let path = state
        .storage
        .upload_path(owner, Uuid::parse_str(&operation).unwrap());
    let mut wrong_hash = raw.clone();
    wrong_hash[0] ^= 1;
    let mut oversized = raw.clone();
    oversized.push(0);
    for (body, expected) in [
        (wrong_hash, ErrorCode::InvalidMessage),
        (oversized, ErrorCode::LimitExceeded),
        (raw[..raw.len() - 1].to_vec(), ErrorCode::InvalidMessage),
    ] {
        let rejected = http
            .put(format!("{base}{}", grant.relative_path))
            .bearer_auth(&client.scope.session_id)
            .header("x-omnimorph-epoch", 1)
            .header("content-type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .unwrap();
        error(
            Envelope::decode(rejected.bytes().await.unwrap())
                .unwrap()
                .body
                .unwrap(),
            expected,
        );
        assert!(!path.exists(), "failed raw transfer left a temporary file");
    }
    let uploaded = http
        .put(format!("{base}{}", grant.relative_path))
        .bearer_auth(&client.scope.session_id)
        .header("x-omnimorph-epoch", 1)
        .header("content-type", "application/octet-stream")
        .body(raw.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(uploaded.status(), 204);
    error(
        client
            .rpc(Body::CommitUpload(CommitUpload {
                operation_id: operation,
                raw_sha256: crate::storage::sha256(&raw),
                raw_length: raw.len() as u64,
            }))
            .await,
        ErrorCode::InvalidMessage,
    );
    assert!(
        !path.exists(),
        "failed container commit left a temporary file"
    );
    assert!(state.storage.avatar(owner, "failed", 1).is_err());
    assert!(state.storage.equipped(owner).unwrap().primary.is_none());
    assert!(matches!(
        client.rpc(Body::UploadRequest(third)).await,
        Body::TransferGrant(_)
    ));
}

async fn upload_resource(
    client: &mut Client,
    descriptor: AvatarDescriptor,
    bytes: &[u8],
) -> AvatarDescriptor {
    use envelope::Body;
    let operation = resource::new_operation_id();
    let Body::TransferGrant(grant) = client
        .rpc(Body::UploadRequest(UploadRequest {
            operation_id: operation.clone(),
            resource: Some(descriptor),
        }))
        .await
    else {
        panic!("missing grant");
    };
    let uploaded = client
        .http
        .put(format!("{}{}", client.base, grant.relative_path))
        .bearer_auth(&client.scope.session_id)
        .header("x-omnimorph-epoch", client.epoch)
        .header("content-type", "application/octet-stream")
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(uploaded.status(), 204);
    let commit = CommitUpload {
        operation_id: operation,
        raw_sha256: crate::storage::sha256(bytes),
        raw_length: bytes.len() as u64,
    };
    let Body::Avatar(avatar) = client.rpc(Body::CommitUpload(commit.clone())).await else {
        panic!("missing commit");
    };
    assert_eq!(
        client.rpc(Body::CommitUpload(commit)).await,
        Body::Avatar(avatar.clone())
    );
    avatar
}

async fn ysm_resources(base: &str, http: &reqwest::Client, state: &AppState, figura: &[u8]) {
    use envelope::Body;
    let owner = Uuid::from_u128(401);
    state
        .user_manager
        .insert(
            owner,
            "ysm-owner".into(),
            Userinfo {
                uuid: owner,
                ..Default::default()
            },
        )
        .unwrap();
    let mut client = Client::login(base, http, "ysm-owner", 1).await;
    // Preserve a separate Figura projection before selecting a YSM primary.
    assert_eq!(
        http.put(format!("{base}/api/avatar"))
            .header("token", "ysm-owner")
            .body(figura.to_vec())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let figura_equipped = state.storage.equipped(owner).unwrap();
    let mut revision = figura_equipped.revision;
    for encrypted in [false, true] {
        let (bytes, content_hash) = crate::service::package::tests::fixture(encrypted, false);
        let model_type = if encrypted {
            ModelType::ModelYsmEncrypted
        } else {
            ModelType::ModelYsmPlain
        };
        let descriptor = AvatarDescriptor {
            owner: owner.to_string(),
            asset_id: if encrypted { "encrypted" } else { "plain" }.into(),
            model_type: model_type as i32,
            distribution_format: DistributionFormat::YsmDistributionPackage as i32,
            raw_sha256: crate::storage::sha256(&bytes),
            raw_length: bytes.len() as u64,
            content_sha256: content_hash,
            schema_sha256: crate::storage::sha256(b"synthetic-schema"),
            required_capabilities: vec![model_type as i32],
            ..Default::default()
        };
        let avatar = upload_resource(&mut client, descriptor, &bytes).await;
        assert_eq!(avatar.model_type, model_type as i32);
        assert_ne!(avatar.raw_sha256, avatar.content_sha256);
        let Body::Equipped(equipped) = client
            .rpc(Body::Equip(Equip {
                operation_id: resource::new_operation_id(),
                asset_id: avatar.asset_id.clone(),
                asset_revision: avatar.revision,
                expected_revision: revision,
                retain_figura_compatibility: !encrypted,
                ..Default::default()
            }))
            .await
        else {
            panic!("missing equipment");
        };
        assert_eq!(equipped.primary.as_ref(), Some(&avatar));
        assert_eq!(equipped.figura_compatibility.is_some(), !encrypted);
        let profile: serde_json::Value = http
            .get(format!("{base}/api/{owner}"))
            .header("token", "legacy-watcher")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            profile["equipped"].as_array().unwrap().len(),
            usize::from(!encrypted)
        );
        let old = http
            .get(format!("{base}/api/{owner}/avatar"))
            .header("token", "legacy-watcher")
            .send()
            .await
            .unwrap();
        if encrypted {
            assert_eq!(old.status(), 404);
        } else {
            assert_eq!(old.bytes().await.unwrap().as_ref(), figura);
        }

        // The extended Figura adapter opts in explicitly; original clients never get YSM bytes.
        let extended = http
            .get(format!("{base}/api/{owner}/avatar"))
            .header("token", "legacy-watcher")
            .header("x-omnimorph-models", "ysm")
            .send()
            .await
            .unwrap();
        assert_eq!(extended.bytes().await.unwrap().as_ref(), bytes);
        let Body::TransferGrant(grant) = client
            .rpc(Body::DownloadRequest(DownloadRequest {
                owner: owner.to_string(),
                asset_id: avatar.asset_id.clone(),
                revision: avatar.revision,
            }))
            .await
        else {
            panic!("missing download grant");
        };
        let downloaded = client.file(&grant.relative_path).await;
        assert_eq!(
            downloaded.headers()["content-type"],
            "application/octet-stream"
        );
        assert_eq!(downloaded.bytes().await.unwrap().as_ref(), bytes);
        assert!(matches!(
            client
                .rpc(Body::Delete(Delete {
                    operation_id: resource::new_operation_id(),
                    asset_id: avatar.asset_id,
                    expected_revision: avatar.revision,
                }))
                .await,
            Body::Ack(_)
        ));
        assert_eq!(client.file(&grant.relative_path).await.status(), 404);
        assert!(state.storage.equipped(owner).unwrap().primary.is_none());
        assert_eq!(
            http.put(format!("{base}/api/avatar"))
                .header("token", "ysm-owner")
                .header("content-type", "application/octet-stream")
                .body(bytes.clone())
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let imported = state.storage.equipped(owner).unwrap();
        assert_eq!(
            imported.primary.as_ref().unwrap().model_type,
            model_type as i32
        );
        assert_eq!(
            imported.primary.as_ref().unwrap().raw_sha256,
            crate::storage::sha256(&bytes)
        );
        assert!(imported.figura_compatibility.is_none());
        let legacy = http
            .get(format!("{base}/api/{owner}/avatar"))
            .header("token", "legacy-watcher")
            .header("x-omnimorph-models", "ysm")
            .send()
            .await
            .unwrap();
        assert_eq!(legacy.bytes().await.unwrap().as_ref(), bytes);
        assert_eq!(
            http.get(format!("{base}/api/{owner}/avatar"))
                .header("token", "legacy-watcher")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(
            http.delete(format!("{base}/api/avatar"))
                .header("token", "ysm-owner")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        revision = state.storage.equipped(owner).unwrap().revision;
    }
}
