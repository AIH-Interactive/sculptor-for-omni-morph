use super::*;

fn vectors() -> HashMap<String, Vec<u8>> {
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/protocol/m7-vectors.tsv"
    ))
    .lines()
    .filter(|line| !line.starts_with('#') && !line.is_empty())
    .map(|line| {
        let (name, hex) = line.split_once('\t').unwrap();
        let mut bytes = vec![0; hex.len() / 2];
        faster_hex::hex_decode(hex.as_bytes(), &mut bytes).unwrap();
        (name.to_owned(), bytes)
    })
    .collect()
}

fn expected_snapshot() -> Frame {
    Frame {
        identity: YsmIdentity {
            owner: Uuid::from_u128(1).to_string(),
            publisher_session: Uuid::from_u128(2).to_string(),
            asset_revision: 1,
            generation: 3,
            sequence: 10,
            base_sequence: -1,
            owner_tick: 20,
            content_sha256: vec![0x11; 32],
            schema_sha256: vec![0x22; 32],
            event_watermark: 2,
        },
        snapshot: true,
        state: YsmState {
            texture: Some(-1),
            controls: Some(Controls {
                values: vec![
                    Control {
                        index: 0,
                        r#type: 1,
                        value: 1.0,
                    },
                    Control {
                        index: 1,
                        r#type: 2,
                        value: 1.25,
                    },
                    Control {
                        index: 2,
                        r#type: 4,
                        value: u32::MAX as f64,
                    },
                ],
            }),
            actions: Some(Actions {
                values: vec![Action {
                    index: 3,
                    active: true,
                    time: 2.5,
                    speed: -1.0,
                    r#loop: true,
                }],
            }),
            controllers: Some(Controllers {
                values: vec![Controller {
                    index: 4,
                    state: 5,
                    time: 6.5,
                }],
            }),
            playbacks: Some(Playbacks {
                values: vec![Playback {
                    slot: 6,
                    clip: 7,
                    controller: 4,
                    time: 1.5,
                    speed: 0.5,
                    weight: 0.25,
                    target_weight: 1.0,
                    loop_mode: 2,
                    fade_speed: 0.75,
                    flags: 7,
                }],
            }),
        },
        events: vec![
            Event {
                sequence: 1,
                kind: 1,
                arguments: vec![0.5, -1.25],
            },
            Event {
                sequence: 2,
                kind: 2,
                arguments: vec![7.0, 2.0, 0.5, 0.25],
            },
        ],
    }
}

#[test]
fn production_java_vectors_match_rust_fields_encoders_and_fragment_crc() {
    let vectors = vectors();
    let snapshot = expected_snapshot();
    assert_eq!(decode(&vectors["snapshot"], 1).unwrap(), snapshot);
    assert_eq!(encode(&snapshot).unwrap(), vectors["snapshot"]);
    let mut delta = snapshot.clone();
    delta.identity.sequence = 11;
    delta.identity.base_sequence = 10;
    delta.identity.owner_tick = 21;
    delta.snapshot = false;
    delta.state = YsmState {
        controls: Some(Controls::default()),
        ..Default::default()
    };
    delta.events.clear();
    assert_eq!(decode(&vectors["delta"], 1).unwrap(), delta);
    assert_eq!(encode(&delta).unwrap(), vectors["delta"]);
    let mut event = delta;
    event.identity.sequence = 12;
    event.identity.base_sequence = 11;
    event.identity.owner_tick = 22;
    event.identity.event_watermark = 3;
    event.state = YsmState::default();
    event.events = vec![Event {
        sequence: 3,
        kind: 4,
        arguments: vec![3.0],
    }];
    assert_eq!(decode(&vectors["event_delta"], 1).unwrap(), event);
    assert_eq!(encode(&event).unwrap(), vectors["event_delta"]);
    assert_eq!(
        fragment(0x0102030405060708, &vectors["snapshot"]).unwrap(),
        vec![vectors["snapshot_packet_0"].clone()]
    );
    let mut incoming = Reassembly::default();
    assert_eq!(
        incoming
            .accept(Uuid::from_u128(1), &vectors["snapshot_packet_0"])
            .unwrap(),
        Some(vectors["snapshot"].clone())
    );
}

#[test]
fn legacy_combined_frames_commit_typed_state_and_events_once_across_restart() {
    let vectors = vectors();
    let directory = tempfile::tempdir().unwrap();
    let storage = crate::storage::Storage::open(directory.path()).unwrap();
    let owner = Uuid::from_u128(1);
    storage
        .import_legacy(
            AvatarDescriptor {
                owner: owner.to_string(),
                asset_id: "avatar".into(),
                model_type: 2,
                distribution_format: 2,
                content_sha256: vec![0x11; 32],
                raw_sha256: vec![1; 32],
                raw_length: 100,
                ..Default::default()
            },
            true,
        )
        .unwrap();
    let key = publisher_key("instance", "legacy-connection", 1);
    let first = storage
        .publish_legacy_ysm(owner, &key, &vectors["snapshot"], 1000)
        .unwrap();
    assert_eq!(first.len(), 1, "initial historical events must not replay");
    assert_eq!(identity(&first[0]).unwrap().sequence, 0);
    assert_eq!(identity(&first[0]).unwrap().event_watermark, 2);
    assert!(storage
        .publish_legacy_ysm(owner, &key, &vectors["snapshot"], 1001)
        .unwrap()
        .is_empty());
    storage
        .publish_legacy_ysm(owner, &key, &vectors["delta"], 1002)
        .unwrap();
    let events = storage
        .publish_legacy_ysm(owner, &key, &vectors["event_delta"], 1003)
        .unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], envelope::Body::YsmDelta(_)));
    assert_eq!(identity(&events[0]).unwrap().sequence, 2);
    assert!(
        matches!(&events[1], envelope::Body::YsmEvent(value) if value.events.len() == 1 && value.events[0].sequence == 3)
    );
    assert_eq!(identity(&events[1]).unwrap().sequence, 3);
    assert!(storage
        .ysm_snapshot(owner, 1004)
        .unwrap()
        .state
        .unwrap()
        .controls
        .unwrap()
        .values
        .is_empty());
    drop(storage);
    let reopened = crate::storage::Storage::open(directory.path()).unwrap();
    assert!(reopened
        .publish_legacy_ysm(owner, &key, &vectors["event_delta"], 1004)
        .unwrap()
        .is_empty());
    let before = reopened.ysm_snapshot(owner, 1004).unwrap();
    let mut wrong_schema = expected_snapshot();
    wrong_schema.identity.sequence = 13;
    wrong_schema.identity.schema_sha256 = vec![0x33; 32];
    assert!(matches!(
        reopened.publish_legacy_ysm(owner, &key, &encode(&wrong_schema).unwrap(), 1005),
        Err(ErrorCode::Conflict)
    ));
    assert_eq!(reopened.ysm_snapshot(owner, 1005).unwrap(), before);
}

#[test]
fn malformed_messages_and_conflicting_or_over_budget_fragments_fail_closed() {
    let vectors = vectors();
    let mut count = vectors["snapshot"].clone();
    count[147..149].copy_from_slice(&513u16.to_be_bytes());
    assert!(matches!(decode(&count, 1), Err(ErrorCode::LimitExceeded)));
    assert!(decode(&vectors["snapshot"][..150], 1).is_err());
    let mut invalid_boolean = vectors["snapshot"].clone();
    invalid_boolean[133] = 2;
    assert!(matches!(
        decode(&invalid_boolean, 1),
        Err(ErrorCode::InvalidMessage)
    ));
    let owner = Uuid::from_u128(1);
    let payload = vec![17; 1000];
    let packets = fragment(7, &payload).unwrap();
    let mut incoming = Reassembly::default();
    assert!(incoming.accept(owner, &packets[0]).unwrap().is_none());
    assert!(incoming.accept(owner, &packets[0]).unwrap().is_none());
    let mut assembled = None;
    for packet in packets.iter().skip(1).rev() {
        if let Some(bytes) = incoming.accept(owner, packet).unwrap() {
            assembled = Some(bytes);
        }
    }
    assert_eq!(assembled, Some(payload.clone()));
    let mut corrupt = packets.clone();
    *corrupt.last_mut().unwrap().last_mut().unwrap() ^= 1;
    let mut bad_crc = Reassembly::default();
    for packet in &corrupt[..corrupt.len() - 1] {
        bad_crc.accept(owner, packet).unwrap();
    }
    assert!(matches!(
        bad_crc.accept(owner, corrupt.last().unwrap()),
        Err(ErrorCode::InvalidMessage)
    ));
    incoming.accept(owner, &packets[0]).unwrap();
    let mut conflicting = packets[0].clone();
    conflicting[25] ^= 1;
    assert!(matches!(
        incoming.accept(owner, &conflicting),
        Err(ErrorCode::Conflict)
    ));
    for id in [8, 9] {
        incoming
            .accept(owner, &fragment(id, &payload).unwrap()[0])
            .unwrap();
    }
    assert!(matches!(
        incoming.accept(owner, &fragment(10, &payload).unwrap()[0]),
        Err(ErrorCode::LimitExceeded)
    ));
    let mut invalid_range = packets[0].clone();
    invalid_range[13..17].copy_from_slice(&32769u32.to_be_bytes());
    assert!(incoming.accept(owner, &invalid_range).is_err());
}
