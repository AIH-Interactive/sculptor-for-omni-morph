use super::*;
use crate::{auth::UManager, service, state::Config, AppState};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::sync::RwLock;

mod rollback;

const CHILD_ENV: &str = "OMNIMORPH_STORAGE_CHILD";
const PAUSE_ENV: &str = "OMNIMORPH_PAUSE_COMMIT";
const OWNER: u128 = 8801;

#[test]
fn registered_profiles_survive_restart_with_model_associations() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("cloud-v1");
    let owner = Uuid::from_u128(8802);
    let storage = Arc::new(Storage::open(&root).unwrap());
    let manager = UManager::open(storage.clone()).unwrap();
    manager
        .insert(
            owner,
            "session-must-not-persist".into(),
            crate::auth::Userinfo {
                uuid: owner,
                nickname: "persistent owner".into(),
                ..Default::default()
            },
        )
        .unwrap();
    manager.put_upload_state(owner, false).unwrap();
    manager
        .ban(&crate::auth::Userinfo {
            uuid: owner,
            ..Default::default()
        })
        .unwrap();
    let avatar = publish(&storage, owner, "ysm");
    drop(manager);
    drop(storage);

    let storage = Arc::new(Storage::open(&root).unwrap());
    let manager = UManager::open(storage.clone()).unwrap();
    let restored = manager
        .get_by_uuid(&owner)
        .expect("refresh by player UUID must still find its profile after server restart");
    assert_eq!(restored.nickname, "persistent owner");
    assert!(
        restored.token.is_none(),
        "authentication tokens must not survive restart"
    );
    assert!(restored.banned);
    assert!(!manager.upload_state(owner, true));
    assert!(manager.get(&"session-must-not-persist".into()).is_none());
    assert_eq!(storage.avatar(owner, "ysm", 0).unwrap(), avatar);
}

#[test]
fn failed_account_commit_does_not_authenticate_or_replace_profile() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Arc::new(Storage::open(directory.path()).unwrap());
    let manager = UManager::open(storage.clone()).unwrap();
    let owner = Uuid::from_u128(8803);
    let original = crate::auth::Userinfo {
        uuid: owner,
        nickname: "original".into(),
        ..Default::default()
    };
    manager.insert_user(owner, original.clone()).unwrap();
    storage.database.lock().unwrap().execute_batch(
        "CREATE TRIGGER reject_account BEFORE INSERT ON users BEGIN SELECT RAISE(ABORT, 'injected account failure'); END;"
    ).unwrap();
    let mut updated = original;
    updated.nickname = "uncommitted".into();
    assert!(manager
        .insert(owner, "uncommitted-token".into(), updated)
        .is_err());
    assert!(manager.get(&"uncommitted-token".into()).is_none());
    assert_eq!(manager.get_by_uuid(&owner).unwrap().nickname, "original");
    storage
        .database
        .lock()
        .unwrap()
        .execute_batch("DROP TRIGGER reject_account")
        .unwrap();
}

#[test]
fn sqlite_imports_manifest_once_and_preserves_uuid_model_links() {
    let directory = tempfile::tempdir().unwrap();
    let old = directory.path().join("old");
    let new = directory.path().join("migrated");
    let owner = Uuid::from_u128(8810);
    let storage = Storage::open(&old).unwrap();
    let avatar = publish(&storage, owner, "future_engine_asset");
    let exported = serde_json::to_vec(&*storage.committed.lock().unwrap()).unwrap();
    fs::create_dir_all(&new).unwrap();
    fs::write(new.join("manifest.json"), &exported).unwrap();
    let migrated = Arc::new(Storage::open(&new).unwrap());
    assert_eq!(
        migrated.avatar(owner, "future_engine_asset", 0).unwrap(),
        avatar
    );
    assert_eq!(fs::read(new.join("manifest.json")).unwrap(), exported);
    let manager = UManager::open(migrated.clone()).unwrap();
    assert!(
        manager.get_by_uuid(&owner).is_some(),
        "old model owners acquire durable profiles"
    );
    let database = migrated.database.lock().unwrap();
    let count: i64 = database
        .query_row(
            "SELECT count(*) FROM avatars WHERE owner_uuid = ?1 AND asset_id = ?2",
            rusqlite::params![owner.to_string(), "future_engine_asset"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "UUID/model association is queryable in SQLite");
    assert!(database
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query([])
        .unwrap()
        .next()
        .unwrap()
        .is_none());
    drop(database);
    drop(manager);
    drop(migrated);
    fs::write(
        new.join("manifest.json"),
        b"obsolete source is no longer authoritative",
    )
    .unwrap();
    assert_eq!(
        Storage::open(&new)
            .unwrap()
            .avatar(owner, "future_engine_asset", 0)
            .unwrap(),
        avatar
    );
}

// Test-only crash point inside the real SQLite transaction, immediately before COMMIT.
pub(super) fn pause_before_replace(root: &Path) {
    if std::env::var_os(PAUSE_ENV).is_none() {
        return;
    }
    fs::write(root.join("commit-ready"), b"before-sqlite-commit").unwrap();
    std::thread::sleep(Duration::from_secs(30));
    panic!("Parent did not terminate the paused commit process");
}

async fn state(root: &Path) -> AppState {
    let storage = Arc::new(Storage::open(root.join("cloud-v1")).unwrap());
    let config: Config = toml::from_str(include_str!("../../Config.example.toml")).unwrap();
    let old_config =
        include_str!("../../Config.example.toml").replace("protobufEnabled = true", "");
    assert!(
        toml::from_str::<Config>(&old_config)
            .unwrap()
            .protobuf_enabled
    );
    AppState {
        uptime: tokio::time::Instant::now(),
        user_manager: Arc::new(UManager::new()),
        session: Arc::new(service::session::SessionRegistry::default()),
        subscribes: Arc::new(service::event::EventHub::default()),
        protocol_sessions: Arc::new(service::session::ProtocolSessions::new(
            storage.instance_id(),
        )),
        storage,
        transfers: Arc::new(service::resource::Transfers::default()),
        config: Arc::new(RwLock::new(config)),
        figura_versions: Arc::new(RwLock::new(None)),
    }
}

fn figura() -> Vec<u8> {
    let text = include_str!("../../protocol/figura-nbt.hex").trim();
    let mut bytes = vec![0; text.len() / 2];
    faster_hex::hex_decode(text.as_bytes(), &mut bytes).unwrap();
    bytes
}

fn descriptor(owner: Uuid, id: &str, bytes: &[u8], content: Vec<u8>) -> AvatarDescriptor {
    AvatarDescriptor {
        owner: owner.to_string(),
        asset_id: id.into(),
        model_type: 2,
        distribution_format: 2,
        raw_sha256: sha256(bytes),
        raw_length: bytes.len() as u64,
        content_sha256: content,
        schema_sha256: vec![6; 32],
        ..Default::default()
    }
}

fn publish(storage: &Storage, owner: Uuid, id: &str) -> AvatarDescriptor {
    let (bytes, content) = service::package::tests::fixture(false, false);
    let operation =
        service::resource::operation_id(&service::resource::new_operation_id()).unwrap();
    let path = storage.upload_path(owner, operation);
    fs::write(&path, &bytes).unwrap();
    storage.publish_staged(&path, &sha256(&bytes)).unwrap();
    storage
        .commit_avatar(
            owner,
            &operation.to_string(),
            id.as_bytes(),
            descriptor(owner, id, &bytes, content),
            10,
        )
        .unwrap()
}

#[tokio::test]
async fn migration_child() {
    let Ok(phase) = std::env::var(CHILD_ENV) else {
        return; // The parent invokes this exact harness in isolated OS processes.
    };
    let root = PathBuf::from(std::env::var_os(crate::AVATARS_ENV).unwrap());
    let owner = Uuid::from_u128(OWNER);
    let state = state(&root).await;
    let legacy = root.join(format!("{owner}.moon"));
    let raw = figura();
    match phase.as_str() {
        "import" => {
            fs::write(&legacy, &raw).unwrap();
            for _ in 0..3 {
                service::resource::ensure_legacy_import(&state, owner, false)
                    .await
                    .unwrap();
            }
            let imported = state.storage.avatar(owner, "avatar", 0).unwrap();
            assert_eq!(imported.revision, 1);
            assert_eq!(imported.raw_sha256, sha256(&raw));
            assert_eq!(fs::read(&legacy).unwrap(), raw);
            let primary = publish(&state.storage, owner, "ysm");
            state
                .storage
                .equip(
                    owner,
                    &Equip {
                        operation_id: service::resource::new_operation_id(),
                        asset_id: primary.asset_id,
                        asset_revision: primary.revision,
                        expected_revision: state.storage.equipped(owner).unwrap().revision,
                        retain_figura_compatibility: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            let removed = publish(&state.storage, owner, "deleted");
            state
                .storage
                .delete(
                    owner,
                    &Delete {
                        operation_id: service::resource::new_operation_id(),
                        asset_id: removed.asset_id,
                        expected_revision: removed.revision,
                    },
                )
                .unwrap();
            let primary = state.storage.equipped(owner).unwrap().primary.unwrap();
            let identity = crate::protocol::v1::YsmIdentity {
                owner: owner.to_string(),
                asset_revision: primary.revision,
                publisher_session: Uuid::from_u128(8802).to_string(),
                generation: 1,
                sequence: 0,
                base_sequence: -1,
                owner_tick: 5,
                content_sha256: primary.content_sha256,
                schema_sha256: primary.schema_sha256,
                ..Default::default()
            };
            let snapshot = crate::protocol::v1::envelope::Body::YsmSnapshot(
                crate::protocol::v1::YsmSnapshot {
                    identity: Some(identity.clone()),
                    state: Some(crate::protocol::v1::YsmState {
                        texture: Some(2),
                        controls: Some(Default::default()),
                        actions: Some(Default::default()),
                        controllers: Some(Default::default()),
                        playbacks: Some(Default::default()),
                    }),
                },
            );
            let key =
                service::ysm::publisher_key(&state.storage.instance_id(), "synthetic-publisher", 1);
            state
                .storage
                .publish_ysm(owner, &key, &snapshot, now_seconds())
                .unwrap();
            let mut event_identity = identity;
            event_identity.sequence = 1;
            event_identity.base_sequence = 0;
            event_identity.event_watermark = 1;
            let event =
                crate::protocol::v1::envelope::Body::YsmEvent(crate::protocol::v1::YsmEvent {
                    identity: Some(event_identity),
                    events: vec![crate::protocol::v1::Event {
                        sequence: 1,
                        kind: crate::protocol::v1::EventKind::Action as i32,
                        arguments: vec![0.0],
                    }],
                });
            state
                .storage
                .publish_ysm(owner, &key, &event, now_seconds())
                .unwrap();
            fs::write(root.join("instance.txt"), state.storage.instance_id()).unwrap();
        }
        "interrupt" => {
            fs::write(
                state.storage.upload_path(
                    owner,
                    service::resource::operation_id(&service::resource::new_operation_id())
                        .unwrap(),
                ),
                b"incomplete-upload",
            )
            .unwrap();
            std::env::set_var(PAUSE_ENV, "1");
            publish(&state.storage, owner, "interrupted");
            panic!("Commit should be terminated by the parent");
        }
        "recover" => {
            assert_eq!(
                state.storage.instance_id(),
                fs::read_to_string(root.join("instance.txt")).unwrap()
            );
            let equipped = state.storage.equipped(owner).unwrap();
            assert_eq!(equipped.primary.unwrap().asset_id, "ysm");
            assert_eq!(equipped.figura_compatibility.unwrap().asset_id, "avatar");
            assert_eq!(
                state.storage.avatar(owner, "interrupted", 0),
                Err(ErrorCode::NotFound)
            );
            assert_eq!(
                state.storage.avatar(owner, "deleted", 0),
                Err(ErrorCode::NotFound)
            );
            assert_eq!(
                fs::read_dir(root.join("cloud-v1/uploads")).unwrap().count(),
                0
            );
            for _ in 0..3 {
                service::resource::ensure_legacy_import(&state, owner, false)
                    .await
                    .unwrap();
            }
            assert_eq!(
                state.storage.avatar(owner, "avatar", 0).unwrap().revision,
                1
            );
            assert_eq!(
                state
                    .storage
                    .equipped(owner)
                    .unwrap()
                    .primary
                    .unwrap()
                    .asset_id,
                "ysm"
            );
            assert_eq!(fs::read(&legacy).unwrap(), raw);
            let snapshot = state.storage.ysm_snapshot(owner, now_seconds()).unwrap();
            assert_eq!(snapshot.state.unwrap().texture, Some(2));
            assert_eq!(snapshot.identity.unwrap().event_watermark, 1);
        }
        _ => panic!("Unknown migration child phase"),
    }
}

fn child(root: &Path, phase: &str) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "storage::tests::migration_child", "--nocapture"])
        .env(CHILD_ENV, phase)
        .env(crate::AVATARS_ENV, root)
        .env_remove(PAUSE_ENV)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

fn completed(root: &Path, phase: &str) {
    let mut process = child(root, phase);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            assert!(status.success(), "Migration child failed: {phase}");
            break;
        }
        if std::time::Instant::now() >= deadline {
            process.kill().unwrap();
            process.wait().unwrap();
            panic!("Migration child timed out: {phase}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn additive_migration_survives_process_termination() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    completed(root, "import");
    let mut interrupted = child(root, "interrupt");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !root.join("cloud-v1/commit-ready").exists() {
        if interrupted.try_wait().unwrap().is_some() || std::time::Instant::now() >= deadline {
            let _ = interrupted.kill();
            let _ = interrupted.wait();
            panic!("Child did not reach the real SQLite pre-commit crash point");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    interrupted.kill().unwrap();
    assert!(!interrupted.wait().unwrap().success());
    let database = rusqlite::Connection::open(root.join("cloud-v1/metadata.sqlite3")).unwrap();
    let interrupted: i64 = database
        .query_row(
            "SELECT count(*) FROM avatars WHERE asset_id = 'interrupted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        interrupted, 0,
        "terminated SQLite transaction must be invisible"
    );
    drop(database);
    completed(root, "recover");
    completed(root, "recover");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    if let Some(binary) = std::env::var_os("OMNIMORPH_ROLLBACK_BINARY") {
        runtime.block_on(rollback::verify(root, Path::new(&binary), None));
        completed(root, "recover");
    } else {
        println!("SKIPPED old production process rollback: set OMNIMORPH_ROLLBACK_BINARY");
    }
    if let Some(binary) = std::env::var_os("OMNIMORPH_DELIVERY_BINARY") {
        runtime.block_on(rollback::verify(root, Path::new(&binary), Some(false)));
        runtime.block_on(rollback::verify(root, Path::new(&binary), Some(true)));
        completed(root, "recover");
    } else {
        println!("SKIPPED production main route toggle: set OMNIMORPH_DELIVERY_BINARY");
    }
    let database = rusqlite::Connection::open(root.join("cloud-v1/metadata.sqlite3")).unwrap();
    database.pragma_update(None, "user_version", library::SCHEMA_VERSION + 1).unwrap();
    let future_upload = root.join("cloud-v1/uploads/future-schema.part");
    fs::write(&future_upload, b"preserve-unknown-data").unwrap();
    assert_eq!(
        Storage::open(root.join("cloud-v1")).unwrap_err(),
        ErrorCode::UnsupportedVersion
    );
    assert_eq!(fs::read(&future_upload).unwrap(), b"preserve-unknown-data");
    assert_eq!(
        database
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        library::SCHEMA_VERSION + 1
    );
    database.pragma_update(None, "user_version", library::SCHEMA_VERSION).unwrap();
    drop(database);
    completed(root, "recover");
    println!("SQLite process termination: additive idempotent legacy import, transaction rollback, restart, tombstones, equipment and version rejection PASS");
}
