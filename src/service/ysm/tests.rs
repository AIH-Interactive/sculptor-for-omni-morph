use super::*;
use crate::storage::Storage;

const NOW: u64 = 1000;

fn equipment(storage: &Storage, owner: Uuid) -> EquippedState {
    let operation = Uuid::from_u128(101).to_string();
    let avatar = storage
        .commit_avatar(
            owner,
            &operation,
            b"fixture",
            AvatarDescriptor {
                owner: owner.to_string(),
                asset_id: "ysm".into(),
                model_type: 2,
                distribution_format: 2,
                raw_sha256: vec![1; 32],
                raw_length: 100,
                content_sha256: vec![2; 32],
                schema_sha256: vec![3; 32],
                ..Default::default()
            },
            5,
        )
        .unwrap();
    storage
        .equip(
            owner,
            &Equip {
                operation_id: Uuid::from_u128(102).to_string(),
                asset_id: avatar.asset_id,
                asset_revision: avatar.revision,
                expected_revision: 0,
                ..Default::default()
            },
        )
        .unwrap()
}

fn snapshot_body(owner: Uuid, publisher: Uuid) -> envelope::Body {
    envelope::Body::YsmSnapshot(YsmSnapshot {
        identity: Some(YsmIdentity {
            owner: owner.to_string(),
            asset_revision: 1,
            publisher_session: publisher.to_string(),
            generation: 1,
            sequence: 0,
            base_sequence: -1,
            owner_tick: 10,
            content_sha256: vec![2; 32],
            schema_sha256: vec![3; 32],
            event_watermark: 0,
        }),
        state: Some(YsmState {
            texture: Some(0),
            controls: Some(Controls {
                values: vec![Control {
                    index: 0,
                    r#type: ControlType::Number as i32,
                    value: 4.0,
                }],
            }),
            actions: Some(Actions::default()),
            controllers: Some(Controllers::default()),
            playbacks: Some(Playbacks {
                values: vec![Playback {
                    controller: -1,
                    speed: 1.0,
                    weight: 1.0,
                    target_weight: 1.0,
                    ..Default::default()
                }],
            }),
        }),
    })
}

fn delta_body(snapshot: &envelope::Body, sequence: u64) -> envelope::Body {
    let mut id = identity(snapshot).unwrap().clone();
    id.sequence = sequence;
    id.base_sequence = sequence as i64 - 1;
    id.owner_tick += sequence;
    envelope::Body::YsmDelta(YsmDelta {
        identity: Some(id),
        state: Some(YsmState {
            texture: Some(1),
            playbacks: Some(Playbacks::default()),
            ..Default::default()
        }),
    })
}

#[test]
fn durable_state_presence_events_resume_and_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path()).unwrap();
    let owner = Uuid::from_u128(1);
    let equipped = equipment(&storage, owner);
    let snapshot = snapshot_body(owner, Uuid::from_u128(2));
    let key = publisher_key(&storage.instance_id(), "secret-auth-token", 1);
    let delta = delta_body(&snapshot, 1);
    assert!(matches!(
        storage.publish_ysm(owner, &key, &delta, NOW),
        Err(ErrorCode::SnapshotRequired)
    ));
    assert!(
        storage
            .publish_ysm(owner, &key, &snapshot, NOW)
            .unwrap()
            .changed
    );
    assert!(
        !storage
            .publish_ysm(owner, &key, &snapshot, NOW)
            .unwrap()
            .changed
    );
    assert!(matches!(
        storage.publish_ysm(owner, &key, &delta_body(&snapshot, 2), NOW),
        Err(ErrorCode::SnapshotRequired)
    ));
    storage.publish_ysm(owner, &key, &delta, NOW + 1).unwrap();
    let mut event_identity = identity(&delta).unwrap().clone();
    event_identity.sequence = 2;
    event_identity.base_sequence = 1;
    event_identity.event_watermark = 1;
    let event = envelope::Body::YsmEvent(YsmEvent {
        identity: Some(event_identity),
        events: vec![Event {
            sequence: 1,
            kind: EventKind::Action as i32,
            arguments: vec![0.0],
        }],
    });
    storage.publish_ysm(owner, &key, &event, NOW + 2).unwrap();
    assert!(
        !storage
            .publish_ysm(owner, &key, &delta, NOW + 3)
            .unwrap()
            .changed
    );
    let latest = storage.ysm_snapshot(owner, NOW + 3).unwrap();
    let state = latest.state.as_ref().unwrap();
    assert_eq!(state.texture, Some(1));
    assert_eq!(
        state.controls.as_ref().unwrap().values.len(),
        1,
        "omission must preserve a partition"
    );
    assert!(
        state.playbacks.as_ref().unwrap().values.is_empty(),
        "present empty must clear a partition"
    );
    assert_eq!(latest.identity.as_ref().unwrap().event_watermark, 1);
    let cursor = Resume {
        owner: owner.to_string(),
        server_epoch: 7,
        revision: 1,
        publisher_session: identity(&snapshot).unwrap().publisher_session.clone(),
        generation: 1,
        sequence: 0,
        event_watermark: 0,
    };
    assert_eq!(
        storage.ysm_resume(&cursor, 7, NOW + 3).unwrap(),
        vec![delta.clone(), event]
    );
    assert!(matches!(
        storage.ysm_resume(&cursor, 8, NOW + 3),
        Err(ErrorCode::CursorExpired)
    ));
    assert!(matches!(
        storage.ysm_resume(&cursor, 7, NOW + 32),
        Err(ErrorCode::SnapshotRequired)
    ));
    let metadata = std::fs::read(directory.path().join("metadata.sqlite3")).unwrap();
    assert!(!metadata
        .windows(b"secret-auth-token".len())
        .any(|bytes| bytes == b"secret-auth-token"));
    drop(storage);
    let reopened = Storage::open(directory.path()).unwrap();
    assert_eq!(
        reopened.ysm_snapshot(owner, NOW + 20).unwrap(),
        latest,
        "offline recovery cannot advance the owner clock"
    );
    assert!(
        matches!(
            reopened.ysm_snapshot(owner, NOW + SNAPSHOT_TTL),
            Err(ErrorCode::Offline)
        ),
        "deltas/events must not extend full-snapshot TTL"
    );
    assert_eq!(equipped, reopened.equipped(owner).unwrap());
}

#[test]
fn authority_takeover_old_generation_and_equipment_revocation_are_durable() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path()).unwrap();
    let owner = Uuid::from_u128(1);
    let equipped = equipment(&storage, owner);
    let first = snapshot_body(owner, Uuid::from_u128(2));
    let second = snapshot_body(owner, Uuid::from_u128(3));
    let key_a = publisher_key("instance", "scope-a", 1);
    let key_b = publisher_key("instance", "scope-b", 1);
    storage.publish_ysm(owner, &key_a, &first, NOW).unwrap();
    storage
        .publish_ysm(owner, &key_b, &second, NOW + 1)
        .unwrap();
    assert!(matches!(
        storage.publish_ysm(owner, &key_a, &delta_body(&first, 1), NOW + 2),
        Err(ErrorCode::Forbidden)
    ));
    assert!(matches!(
        storage.publish_ysm(
            owner,
            &key_a,
            &snapshot_body(owner, Uuid::from_u128(4)),
            NOW + 2
        ),
        Err(ErrorCode::Forbidden)
    ));
    let mut next_generation = second.clone();
    if let envelope::Body::YsmSnapshot(value) = &mut next_generation {
        value.identity.as_mut().unwrap().generation = 2;
    }
    storage
        .publish_ysm(owner, &key_b, &next_generation, NOW + 2)
        .unwrap();
    assert!(matches!(
        storage.publish_ysm(owner, &key_b, &second, NOW + 3),
        Err(ErrorCode::Conflict)
    ));
    let mut forged = next_generation.clone();
    if let envelope::Body::YsmSnapshot(value) = &mut forged {
        value.identity.as_mut().unwrap().owner = Uuid::from_u128(99).to_string();
    }
    assert!(matches!(
        storage.publish_ysm(owner, &key_b, &forged, NOW + 3),
        Err(ErrorCode::Forbidden)
    ));
    storage
        .equip(
            owner,
            &Equip {
                operation_id: Uuid::from_u128(103).to_string(),
                clear: true,
                expected_revision: equipped.revision,
                ..Default::default()
            },
        )
        .unwrap();
    drop(storage);
    let reopened = Storage::open(directory.path()).unwrap();
    assert!(matches!(
        reopened.ysm_snapshot(owner, NOW + 4),
        Err(ErrorCode::SnapshotRequired)
    ));
    assert!(matches!(
        reopened.publish_ysm(owner, &key_b, &delta_body(&next_generation, 1), NOW + 4),
        Err(ErrorCode::NotFound)
    ));
}

#[test]
fn failed_sqlite_commit_does_not_publish_state() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path()).unwrap();
    let owner = Uuid::from_u128(1);
    equipment(&storage, owner);
    let first = snapshot_body(owner, Uuid::from_u128(2));
    let key = publisher_key("instance", "scope", 1);
    storage.publish_ysm(owner, &key, &first, NOW).unwrap();
    let before = storage.ysm_snapshot(owner, NOW).unwrap();
    let database = rusqlite::Connection::open(directory.path().join("metadata.sqlite3")).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER reject_ysm BEFORE INSERT ON ysm_state BEGIN
        SELECT RAISE(ABORT, 'injected SQLite commit failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        storage.publish_ysm(owner, &key, &delta_body(&first, 1), NOW + 1),
        Err(ErrorCode::Internal)
    ));
    assert_eq!(storage.ysm_snapshot(owner, NOW + 1).unwrap(), before);
    database.execute_batch("DROP TRIGGER reject_ysm").unwrap();
    drop(database);
    drop(storage);
    let reopened = Storage::open(directory.path()).unwrap();
    assert_eq!(reopened.ysm_snapshot(owner, NOW + 2).unwrap(), before);
}

#[test]
fn validation_and_tail_eviction_preserve_the_committed_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(directory.path()).unwrap();
    let owner = Uuid::from_u128(1);
    equipment(&storage, owner);
    let first = snapshot_body(owner, Uuid::from_u128(2));
    let key = publisher_key("instance", "scope", 1);
    let mut invalid = first.clone();
    if let envelope::Body::YsmSnapshot(value) = &mut invalid {
        let controls = value.state.as_mut().unwrap().controls.as_mut().unwrap();
        controls.values.push(controls.values[0]);
    }
    assert!(matches!(
        storage.publish_ysm(owner, &key, &invalid, NOW),
        Err(ErrorCode::InvalidMessage)
    ));
    storage.publish_ysm(owner, &key, &first, NOW).unwrap();
    let mut mismatched = delta_body(&first, 1);
    if let envelope::Body::YsmDelta(value) = &mut mismatched {
        value.identity.as_mut().unwrap().schema_sha256 = vec![99; 32];
    }
    assert!(matches!(
        storage.publish_ysm(owner, &key, &mismatched, NOW + 1),
        Err(ErrorCode::Conflict)
    ));
    assert_eq!(
        storage
            .ysm_snapshot(owner, NOW + 1)
            .unwrap()
            .identity
            .unwrap()
            .sequence,
        0
    );
    for sequence in 1..=35 {
        storage
            .publish_ysm(owner, &key, &delta_body(&first, sequence), NOW + sequence)
            .unwrap();
    }
    let cursor = Resume {
        owner: owner.to_string(),
        server_epoch: 7,
        revision: 1,
        publisher_session: identity(&first).unwrap().publisher_session.clone(),
        generation: 1,
        sequence: 0,
        event_watermark: 0,
    };
    assert!(matches!(
        storage.ysm_resume(&cursor, 7, NOW + 36),
        Err(ErrorCode::SnapshotRequired)
    ));
    let recovered = storage.ysm_snapshot(owner, NOW + 36).unwrap();
    assert_eq!(recovered.identity.unwrap().sequence, 35);
    let state = recovered.state.unwrap();
    assert_eq!(state.controls.unwrap().values.len(), 1);
    assert!(state.playbacks.unwrap().values.is_empty());
    storage
        .delete(
            owner,
            &Delete {
                operation_id: Uuid::from_u128(103).to_string(),
                asset_id: "ysm".into(),
                expected_revision: 1,
            },
        )
        .unwrap();
    drop(storage);
    let reopened = Storage::open(directory.path()).unwrap();
    assert!(matches!(
        reopened.ysm_snapshot(owner, NOW + 37),
        Err(ErrorCode::SnapshotRequired)
    ));
}
