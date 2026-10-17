use super::v1::*;
use crate::api::figura::websocket::{C2SMessage, S2CMessage};
use prost::Message;
use std::collections::HashMap;
use uuid::Uuid;

fn vectors() -> HashMap<&'static str, Vec<u8>> {
    include_str!("../../protocol/vectors.tsv")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (name, hex) = line.split_once('\t').unwrap();
            let mut bytes = vec![0; hex.len() / 2];
            faster_hex::hex_decode(hex.as_bytes(), &mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

#[test]
fn java_rust_wire_contract() {
    use envelope::Body;
    let cases = [
        (
            "hello",
            Body::Hello(Hello {
                supported_majors: vec![1],
                client_version: "j".into(),
                ..Default::default()
            }),
        ),
        (
            "ping",
            Body::FiguraPing(FiguraPing {
                function_id: 0x01020304,
                echo: true,
                arguments: vec![0, 255, 128, 127],
                ..Default::default()
            }),
        ),
        (
            "clear_controls",
            Body::YsmDelta(YsmDelta {
                state: Some(YsmState {
                    controls: Some(Controls::default()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ),
    ];
    let vectors = vectors();
    for (index, (name, body)) in cases.into_iter().enumerate() {
        let expected = Envelope {
            major: 1,
            request_id: index as u64 + 1,
            body: Some(body),
            ..Default::default()
        };
        let wire = &vectors[name];
        assert_eq!(&expected.encode_to_vec(), wire, "Rust encoder: {name}");
        assert_eq!(
            Envelope::decode(wire.as_slice()).unwrap(),
            expected,
            "Java vector: {name}"
        );
        let mut extended = wire.clone();
        extended.extend_from_slice(&[0xa0, 6, 7]);
        assert_eq!(Envelope::decode(extended.as_slice()).unwrap(), expected);
    }
    assert!(Envelope::decode([0x52, 5, 1].as_slice()).is_err());
}

#[test]
fn legacy_frames_are_frozen() {
    let owner = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
    let vectors = vectors();
    let c2s = [
        ("legacy_auth", C2SMessage::Token(b"test-token".to_vec())),
        (
            "legacy_ping",
            C2SMessage::Ping(0x01020304, true, vec![0, 255, 128, 127]),
        ),
        ("legacy_sub", C2SMessage::Sub(owner)),
        ("legacy_unsub", C2SMessage::Unsub(owner)),
    ];
    for (name, message) in c2s {
        let wire: Vec<u8> = message.clone().into();
        assert_eq!(wire, vectors[name], "{name}");
        assert_eq!(C2SMessage::try_from(wire.as_slice()).unwrap(), message);
    }
    let s2c = [
        ("legacy_s2c_auth", S2CMessage::Auth),
        (
            "legacy_s2c_ping",
            S2CMessage::Ping(owner, 0x01020304, true, vec![0, 255, 128, 127]),
        ),
        ("legacy_event", S2CMessage::Event(owner)),
        (
            "legacy_toast",
            S2CMessage::Toast(2, "hello".into(), Some("detail".into())),
        ),
        ("legacy_chat", S2CMessage::Chat("{}".into())),
        ("legacy_notice", S2CMessage::Notice(1)),
    ];
    for (name, message) in s2c {
        let wire: Vec<u8> = message.into();
        assert_eq!(wire, vectors[name], "{name}");
    }
}

#[tokio::test]
async fn legacy_http_baseline() {
    // The production legacy roots are initialized once per process. Run this
    // directory-sensitive baseline in its own process so parallel API fixtures
    // cannot initialize those roots before this test installs its environment.
    const CHILD: &str = "OMNIMORPH_LEGACY_HTTP_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "protocol::tests::legacy_http_baseline", "--nocapture"])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(result.success(), "isolated legacy HTTP baseline failed");
        return;
    }
    use crate::{
        auth::{UManager, Userinfo},
        state::Config,
        AppState,
    };
    use axum::{routing::get, Router};
    use std::sync::Arc;
    use tokio::{sync::RwLock, time::Instant};

    // No production resources, accounts, external auth or deployment are accessed.
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var(crate::AVATARS_ENV, directory.path());
    std::env::set_var(crate::ASSETS_ENV, directory.path().join("assets"));
    tokio::fs::create_dir(directory.path().join("assets"))
        .await
        .unwrap();
    let config: Config = toml::from_str(include_str!("../../Config.example.toml")).unwrap();
    let owner = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
    let storage =
        Arc::new(crate::storage::Storage::open(directory.path().join("cloud-v1")).unwrap());
    let manager = UManager::open(storage.clone()).unwrap();
    manager
        .insert(
            owner,
            "baseline-token".into(),
            Userinfo {
                uuid: owner,
                ..Default::default()
            },
        )
        .unwrap();
    let protocol_sessions = Arc::new(crate::service::session::ProtocolSessions::new(
        storage.instance_id(),
    ));
    let state = AppState {
        uptime: Instant::now(),
        user_manager: Arc::new(manager),
        session: Arc::new(crate::service::session::SessionRegistry::default()),
        subscribes: Arc::new(crate::service::event::EventHub::default()),
        storage,
        protocol_sessions,
        transfers: Arc::new(crate::service::resource::Transfers::default()),
        config: Arc::new(RwLock::new(config)),
        figura_versions: Arc::new(RwLock::new(None)),
    };
    let capture = super::capture::Capture::default();
    let router = Router::new()
        .nest("/api", crate::legacy_api(1024))
        .route("/api/", get(crate::check_auth))
        .route("/ws", get(crate::api::figura::ws))
        .merge(crate::api::protobuf::router(true))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            capture.clone(),
            super::capture::requests,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    });
    let client = reqwest::Client::new();
    let base = format!("http://{address}");
    let hex = include_str!("../../protocol/figura-nbt.hex").trim();
    let mut resource = vec![0; hex.len() / 2];
    faster_hex::hex_decode(hex.as_bytes(), &mut resource).unwrap();
    let avatar_path = directory.path().join(format!("{owner}.moon"));
    tokio::fs::write(&avatar_path, &resource).await.unwrap();
    let initial_profile: serde_json::Value = client
        .get(format!("{base}/api/{owner}"))
        .header("token", "baseline-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Legacy self-profile probes the one-shot temp slot before the persistent slot.
    assert_eq!(initial_profile["equipped"], serde_json::json!([]));
    let profile: serde_json::Value = client
        .get(format!("{base}/api/{owner}"))
        .header("token", "baseline-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let raw_hash =
        faster_hex::hex_string(ring::digest::digest(&ring::digest::SHA256, &resource).as_ref());
    assert_eq!(
        raw_hash,
        "51b2e69108970a9a8413720b7e3ebf94177acc8113e188b142c9cd7f0fcad695"
    );
    // Historical identity hashes the base64 text, not the compressed NBT bytes.
    let legacy_hash = "7d8e5d453e49f4875e7536c4f423af386fc1484d229dc904357da8832828cc2b";
    assert_eq!(
        profile["equipped"],
        serde_json::json!([{"id":"avatar","owner":owner.to_string(),"hash":legacy_hash}])
    );
    let downloaded = client
        .get(format!("{base}/api/{owner}/avatar"))
        .header("token", "baseline-token")
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(downloaded.as_ref(), resource.as_slice());
    assert_eq!(tokio::fs::read(&avatar_path).await.unwrap(), resource);
    let response = client
        .get(format!("{base}/api/"))
        .header("token", "baseline-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let response = client
        .get(format!("{base}/api/"))
        .header("token", "invalid")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let limits: serde_json::Value = client
        .get(format!("{base}/api/limits"))
        .header("token", "baseline-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        limits["rate"],
        serde_json::json!({"pingSize":1024,"pingRate":32,"equip":1,"download":50,"upload":1})
    );
    assert_eq!(
        limits["limits"]["allowedBadges"]["special"],
        serde_json::json!([0, 0, 0, 0, 0, 0])
    );
    let response = client
        .get(format!("{base}/api//auth/id?username=baseline"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap().len(), 40);
    let response = client
        .get(format!("{base}/api/auth/id?username=baseline"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let response = client
        .get(format!("{base}/api//assets/missing-resource"))
        .send()
        .await
        .unwrap();
    assert_ne!(
        response.status(),
        404,
        "Historical assets route must still dispatch"
    );
    crate::service::tests::legacy_lifecycle(&base, &client, &state, &resource, owner).await;
    crate::api::protobuf::tests::integration(&base, &client, &state, &resource).await;
    crate::api::protobuf::tests::java_client::integration(&base, &state, &resource).await;
    let restart = crate::api::protobuf::tests::restart::prepare(&base, &client, &state).await;
    capture.assert_raw_upload(&resource, true);
    capture.assert_raw_upload(&resource, false);
    for encrypted in [false, true] {
        let (bytes, _) = crate::service::package::tests::fixture(encrypted, false);
        capture.assert_raw_upload(&bytes, false);
        capture.assert_raw_upload(&bytes, true);
    }
    stop.send(()).unwrap();
    server.await.unwrap();
    restart
        .verify(state.config.clone(), directory.path().join("cloud-v1"))
        .await;
}
