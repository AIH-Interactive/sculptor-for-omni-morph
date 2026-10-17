//! Transport-independent YSM authority, state reduction and bounded recovery.
use crate::protocol::{v1::*, wire::MAX_STATE_BYTES};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

type Result<T> = std::result::Result<T, ErrorCode>;
pub mod legacy;
#[cfg(test)]
mod tests;
pub const SNAPSHOT_TTL: u64 = 300;
const EVENT_TTL: u64 = 30;
const MAX_TAIL_BYTES: usize = 262_144;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Record {
    schema_version: u32,
    pub equipment_revision: u64,
    pub active: bool,
    publisher_key: Vec<u8>,
    original_snapshot: Vec<u8>,
    materialized: Vec<u8>,
    snapshot_received_at: u64,
    last_frame: Vec<u8>,
    tail: Vec<StoredFrame>,
    retired: Vec<Retired>,
    #[serde(default)]
    legacy_origin: Option<LegacyOrigin>,
}

#[derive(Clone, Serialize, Deserialize)]
struct LegacyOrigin {
    scope_key: Vec<u8>,
    publisher: String,
    generation: u64,
    sequence: u64,
    fingerprint: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredFrame {
    sequence: u64,
    received_at: u64,
    events: usize,
    envelope: Vec<u8>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Retired {
    publisher: String,
    scope_key: Vec<u8>,
    expires_at: u64,
}

pub struct Published {
    pub ack: Ack,
    pub changed: bool,
}

/// This is a non-reversible binding, never an authentication credential on disk.
pub fn publisher_key(instance: &str, session: &str, epoch: u64) -> Vec<u8> {
    crate::storage::sha256(
        &[
            instance.as_bytes(),
            &[0],
            session.as_bytes(),
            &epoch.to_be_bytes(),
        ]
        .concat(),
    )
}

pub(crate) fn identity(body: &envelope::Body) -> Result<&YsmIdentity> {
    match body {
        envelope::Body::YsmSnapshot(value) => value.identity.as_ref(),
        envelope::Body::YsmDelta(value) => value.identity.as_ref(),
        envelope::Body::YsmEvent(value) => value.identity.as_ref(),
        _ => None,
    }
    .ok_or(ErrorCode::InvalidMessage)
}

pub fn advance_cursor(baseline: &mut Option<YsmIdentity>, body: &envelope::Body) -> Result<bool> {
    let Ok(id) = identity(body) else {
        return Ok(true);
    };
    let snapshot = matches!(body, envelope::Body::YsmSnapshot(_));
    if let Some(old) = baseline {
        let same = old.publisher_session == id.publisher_session
            && old.generation == id.generation
            && old.asset_revision == id.asset_revision;
        if same && id.sequence <= old.sequence {
            return Ok(false);
        }
        if !snapshot && (!same || id.sequence != old.sequence + 1) {
            return Err(ErrorCode::SnapshotRequired);
        }
    } else if !snapshot {
        return Err(ErrorCode::SnapshotRequired);
    }
    *baseline = Some(id.clone());
    Ok(true)
}

fn encoded(body: &envelope::Body) -> Vec<u8> {
    Envelope {
        major: 1,
        body: Some(body.clone()),
        ..Default::default()
    }
    .encode_to_vec()
}

fn decode_frame(bytes: &[u8]) -> Result<envelope::Body> {
    Envelope::decode(bytes)
        .map_err(|_| ErrorCode::Internal)?
        .body
        .ok_or(ErrorCode::Internal)
}

fn materialized(record: &Record) -> Result<YsmSnapshot> {
    YsmSnapshot::decode(record.materialized.as_slice()).map_err(|_| ErrorCode::Internal)
}

fn validate_identity(owner: Uuid, equipped: &EquippedState, id: &YsmIdentity) -> Result<()> {
    if id.owner.starts_with("entity:") {
        return Err(ErrorCode::AuthorityUnavailable);
    }
    if id.owner != owner.to_string() {
        return Err(ErrorCode::Forbidden);
    }
    let publisher =
        Uuid::parse_str(&id.publisher_session).map_err(|_| ErrorCode::InvalidMessage)?;
    if publisher.to_string() != id.publisher_session
        || id.generation > i64::MAX as u64
        || id.sequence > i64::MAX as u64
        || id.owner_tick > i64::MAX as u64
        || id.event_watermark > i64::MAX as u64
    {
        return Err(ErrorCode::InvalidMessage);
    }
    let avatar = equipped.primary.as_ref().ok_or(ErrorCode::NotFound)?;
    if !matches!(avatar.model_type, 2 | 3) {
        return Err(ErrorCode::UnsupportedCapability);
    }
    if id.asset_revision != avatar.revision
        || id.content_sha256.len() != 32
        || id.schema_sha256.len() != 32
        || id.content_sha256 != avatar.content_sha256
        || (id.schema_sha256 != avatar.schema_sha256
            && !(avatar.asset_id == "avatar" && avatar.schema_sha256.is_empty()))
    {
        return Err(ErrorCode::Conflict);
    }
    Ok(())
}

/// No game-server authority provider is installed. An entity-qualified target
/// is rejected explicitly; it is never interpreted as a client-owned binding.
pub fn player_owner(text: &str) -> Result<Uuid> {
    if text.starts_with("entity:") {
        return Err(ErrorCode::AuthorityUnavailable);
    }
    Uuid::parse_str(text).map_err(|_| ErrorCode::InvalidMessage)
}

fn unique(indices: impl Iterator<Item = u32>) -> Result<()> {
    let mut seen = HashSet::new();
    for index in indices {
        if index > 65535 || !seen.insert(index) {
            return Err(ErrorCode::InvalidMessage);
        }
        if seen.len() > 512 {
            return Err(ErrorCode::LimitExceeded);
        }
    }
    Ok(())
}

fn valid_time(value: f32) -> bool {
    value.is_finite() && value >= 0.0
}

pub fn validate_state(state: &YsmState, complete: bool) -> Result<()> {
    if complete
        && (state.texture.is_none()
            || state.controls.is_none()
            || state.actions.is_none()
            || state.controllers.is_none()
            || state.playbacks.is_none())
    {
        return Err(ErrorCode::InvalidMessage);
    }
    if state
        .texture
        .is_some_and(|index| !(-1..=65535).contains(&index))
    {
        return Err(ErrorCode::InvalidMessage);
    }
    if let Some(controls) = &state.controls {
        unique(controls.values.iter().map(|value| value.index))?;
        for value in &controls.values {
            let valid = match ControlType::try_from(value.r#type) {
                Ok(ControlType::Boolean) => value.value == 0.0 || value.value == 1.0,
                Ok(ControlType::Number) => value.value.is_finite(),
                Ok(ControlType::Option) => {
                    (0.0..=65535.0).contains(&value.value) && value.value.fract() == 0.0
                }
                Ok(ControlType::Color) => {
                    (0.0..=u32::MAX as f64).contains(&value.value) && value.value.fract() == 0.0
                }
                _ => false,
            };
            if !valid {
                return Err(ErrorCode::InvalidMessage);
            }
        }
    }
    if let Some(actions) = &state.actions {
        unique(actions.values.iter().map(|value| value.index))?;
        if actions
            .values
            .iter()
            .any(|value| !valid_time(value.time) || !value.speed.is_finite())
        {
            return Err(ErrorCode::InvalidMessage);
        }
    }
    if let Some(controllers) = &state.controllers {
        unique(controllers.values.iter().map(|value| value.index))?;
        if controllers
            .values
            .iter()
            .any(|value| value.state > 65535 || !valid_time(value.time))
        {
            return Err(ErrorCode::InvalidMessage);
        }
    }
    if let Some(playbacks) = &state.playbacks {
        unique(playbacks.values.iter().map(|value| value.slot))?;
        if playbacks.values.iter().any(|value| {
            value.clip > 65535
                || !(-1..=65535).contains(&value.controller)
                || !valid_time(value.time)
                || !value.speed.is_finite()
                || !valid_time(value.weight)
                || !valid_time(value.target_weight)
                || !valid_time(value.fade_speed)
                || value.loop_mode > 2
                || value.flags > 7
        }) {
            return Err(ErrorCode::InvalidMessage);
        }
    }
    if state.encoded_len() > MAX_STATE_BYTES {
        return Err(ErrorCode::LimitExceeded);
    }
    Ok(())
}

fn merge(target: &mut YsmState, delta: &YsmState) {
    if delta.texture.is_some() {
        target.texture = delta.texture;
    }
    if delta.controls.is_some() {
        target.controls = delta.controls.clone();
    }
    if delta.actions.is_some() {
        target.actions = delta.actions.clone();
    }
    if delta.controllers.is_some() {
        target.controllers = delta.controllers.clone();
    }
    if delta.playbacks.is_some() {
        target.playbacks = delta.playbacks.clone();
    }
}

fn validate_events(events: &[Event], previous: u64, watermark: u64) -> Result<()> {
    if events.is_empty() || events.len() > 64 {
        return Err(ErrorCode::LimitExceeded);
    }
    let mut sequence = previous;
    for event in events {
        sequence = sequence.checked_add(1).ok_or(ErrorCode::InvalidMessage)?;
        if event.sequence != sequence
            || event.arguments.len() > 16
            || event.arguments.iter().any(|value| !value.is_finite())
        {
            return Err(ErrorCode::InvalidMessage);
        }
        let index = |value: f32| (0.0..=65535.0).contains(&value) && value.fract() == 0.0;
        let valid = match EventKind::try_from(event.kind) {
            Ok(EventKind::Sync) => true,
            Ok(EventKind::Play) => {
                event.arguments.len() == 4
                    && index(event.arguments[0])
                    && (0.0..=2.0).contains(&event.arguments[1])
                    && event.arguments[1].fract() == 0.0
                    && event.arguments[3] >= 0.0
            }
            Ok(EventKind::Stop | EventKind::Action) => {
                event.arguments.len() == 1 && index(event.arguments[0])
            }
            _ => false,
        };
        if !valid {
            return Err(ErrorCode::InvalidMessage);
        }
    }
    if sequence != watermark {
        return Err(ErrorCode::Conflict);
    }
    Ok(())
}

/// Call within the store transaction: an error must discard every mutation.
pub fn publish(
    record: &mut Record,
    owner: Uuid,
    equipped: &EquippedState,
    scope_key: &[u8],
    body: &envelope::Body,
    now: u64,
) -> Result<Published> {
    if !record.materialized.is_empty() && record.schema_version != 1 {
        return Err(ErrorCode::UnsupportedVersion);
    }
    let id = identity(body)?;
    validate_identity(owner, equipped, id)?;
    let frame = encoded(body);
    let body_length = match body {
        envelope::Body::YsmSnapshot(value) => value.encoded_len(),
        envelope::Body::YsmDelta(value) => value.encoded_len(),
        envelope::Body::YsmEvent(value) => value.encoded_len(),
        _ => return Err(ErrorCode::InvalidMessage),
    };
    if body_length > MAX_STATE_BYTES {
        return Err(ErrorCode::LimitExceeded);
    }
    record.retired.retain(|entry| entry.expires_at > now);
    if record.retired.iter().any(|entry| {
        entry.publisher == id.publisher_session
            || (!entry.scope_key.is_empty() && entry.scope_key == scope_key)
    }) {
        return Err(ErrorCode::Forbidden);
    }
    let previous = if record.materialized.is_empty() {
        None
    } else {
        Some(materialized(record)?)
    };
    let old_id = previous
        .as_ref()
        .and_then(|snapshot| snapshot.identity.as_ref());
    if equipped
        .primary
        .as_ref()
        .is_some_and(|avatar| avatar.schema_sha256.is_empty())
        && old_id.is_some_and(|old| {
            old.asset_revision == id.asset_revision
                && old.content_sha256 == id.content_sha256
                && old.schema_sha256 != id.schema_sha256
        })
    {
        return Err(ErrorCode::Conflict);
    }
    let continuing_identity = record.publisher_key == scope_key
        && old_id.is_some_and(|old| old.publisher_session == id.publisher_session);
    let same_publisher =
        record.active && record.equipment_revision == equipped.revision && continuing_identity;
    if same_publisher
        && (record.last_frame == frame || record.tail.iter().any(|entry| entry.envelope == frame))
    {
        return Ok(Published {
            ack: Ack {
                revision: equipped.revision,
                sequence: id.sequence,
                ..Default::default()
            },
            changed: false,
        });
    }
    let is_snapshot = matches!(body, envelope::Body::YsmSnapshot(_));
    let new_generation =
        continuing_identity && old_id.is_some_and(|old| id.generation > old.generation);
    let new_authority = !same_publisher || new_generation;
    if new_authority {
        if !is_snapshot || id.sequence != 0 || id.base_sequence != -1 {
            return Err(ErrorCode::SnapshotRequired);
        }
        if let Some(old) = old_id {
            if !same_publisher {
                if continuing_identity {
                    if !new_generation {
                        return Err(ErrorCode::Forbidden);
                    }
                } else {
                    if old.publisher_session == id.publisher_session {
                        return Err(ErrorCode::Forbidden);
                    }
                    if record.retired.len() >= 64 {
                        return Err(ErrorCode::LimitExceeded);
                    }
                    record.retired.push(Retired {
                        publisher: old.publisher_session.clone(),
                        scope_key: if record.active && record.publisher_key != scope_key {
                            record.publisher_key.clone()
                        } else {
                            Vec::new()
                        },
                        expires_at: now.saturating_add(3600),
                    });
                }
            }
        }
    } else {
        let old = old_id.ok_or(ErrorCode::SnapshotRequired)?;
        if id.generation != old.generation
            || id.sequence <= old.sequence
            || id.owner_tick < old.owner_tick
        {
            return Err(ErrorCode::Conflict);
        }
        if !is_snapshot
            && (id.sequence != old.sequence + 1 || id.base_sequence != old.sequence as i64)
        {
            return Err(ErrorCode::SnapshotRequired);
        }
        if record.snapshot_received_at.saturating_add(SNAPSHOT_TTL) <= now && !is_snapshot {
            return Err(ErrorCode::Offline);
        }
    }
    let mut snapshot = previous.clone().unwrap_or_default();
    let mut event_count = 0;
    match body {
        envelope::Body::YsmSnapshot(value) => {
            if id.base_sequence != -1 {
                return Err(ErrorCode::InvalidMessage);
            }
            validate_state(value.state.as_ref().ok_or(ErrorCode::InvalidMessage)?, true)?;
            if !new_authority && old_id.is_some_and(|old| old.event_watermark != id.event_watermark)
            {
                return Err(ErrorCode::SnapshotRequired);
            }
            snapshot = value.clone();
            record.original_snapshot = value.encode_to_vec();
            record.snapshot_received_at = now;
            record.tail.clear();
        }
        envelope::Body::YsmDelta(value) => {
            let delta = value.state.as_ref().ok_or(ErrorCode::InvalidMessage)?;
            validate_state(delta, false)?;
            if old_id.is_some_and(|old| old.event_watermark != id.event_watermark) {
                return Err(ErrorCode::Conflict);
            }
            merge(
                snapshot.state.as_mut().ok_or(ErrorCode::SnapshotRequired)?,
                delta,
            );
        }
        envelope::Body::YsmEvent(value) => {
            validate_events(
                &value.events,
                old_id.ok_or(ErrorCode::SnapshotRequired)?.event_watermark,
                id.event_watermark,
            )?;
            event_count = value.events.len();
        }
        _ => return Err(ErrorCode::InvalidMessage),
    }
    let mut recovery_identity = id.clone();
    recovery_identity.base_sequence = -1;
    snapshot.identity = Some(recovery_identity);
    if snapshot.encoded_len() > MAX_STATE_BYTES {
        return Err(ErrorCode::LimitExceeded);
    }
    if !is_snapshot {
        record.tail.push(StoredFrame {
            sequence: id.sequence,
            received_at: now,
            events: event_count,
            envelope: frame.clone(),
        });
    }
    while record.tail.len() > 32
        || record
            .tail
            .iter()
            .map(|frame| frame.envelope.len())
            .sum::<usize>()
            > MAX_TAIL_BYTES
        || record.tail.iter().map(|frame| frame.events).sum::<usize>() > 64
    {
        record.tail.remove(0);
    }
    record.schema_version = 1;
    record.equipment_revision = equipped.revision;
    record.active = true;
    record.publisher_key = scope_key.to_vec();
    record.materialized = snapshot.encode_to_vec();
    record.last_frame = frame;
    Ok(Published {
        ack: Ack {
            revision: equipped.revision,
            sequence: id.sequence,
            ..Default::default()
        },
        changed: true,
    })
}

pub fn snapshot(record: &Record, equipped: &EquippedState, now: u64) -> Result<YsmSnapshot> {
    if record.schema_version != 1 {
        return Err(ErrorCode::SnapshotRequired);
    }
    if !record.active || record.equipment_revision != equipped.revision {
        return Err(ErrorCode::SnapshotRequired);
    }
    if record.snapshot_received_at.saturating_add(SNAPSHOT_TTL) <= now {
        return Err(ErrorCode::Offline);
    }
    let result = materialized(record)?;
    let id = result.identity.as_ref().ok_or(ErrorCode::Internal)?;
    let owner = Uuid::parse_str(&id.owner).map_err(|_| ErrorCode::Internal)?;
    validate_identity(owner, equipped, id)?;
    Ok(result)
}

pub fn resume(
    record: &Record,
    equipped: &EquippedState,
    cursor: &Resume,
    server_epoch: u64,
    now: u64,
) -> Result<Vec<envelope::Body>> {
    let latest = snapshot(record, equipped, now)?;
    let id = latest.identity.as_ref().ok_or(ErrorCode::Internal)?;
    if cursor.server_epoch != server_epoch {
        return Err(ErrorCode::CursorExpired);
    }
    if cursor.owner != id.owner
        || cursor.revision != id.asset_revision
        || cursor.publisher_session != id.publisher_session
        || cursor.generation != id.generation
        || cursor.sequence > id.sequence
        || cursor.event_watermark > id.event_watermark
    {
        return Err(ErrorCode::SnapshotRequired);
    }
    if cursor.sequence == id.sequence {
        return if cursor.event_watermark == id.event_watermark {
            Ok(Vec::new())
        } else {
            Err(ErrorCode::SnapshotRequired)
        };
    }
    let tail: Vec<_> = record
        .tail
        .iter()
        .filter(|frame| frame.sequence > cursor.sequence)
        .collect();
    let mut sequence = cursor.sequence;
    let mut watermark = cursor.event_watermark;
    let mut result = Vec::new();
    for frame in tail {
        if frame.sequence != sequence + 1
            || (frame.events > 0 && frame.received_at.saturating_add(EVENT_TTL) <= now)
        {
            return Err(ErrorCode::SnapshotRequired);
        }
        let body = decode_frame(&frame.envelope)?;
        let frame_id = identity(&body)?;
        if frame.events > 0 {
            validate_events(
                match &body {
                    envelope::Body::YsmEvent(value) => &value.events,
                    _ => return Err(ErrorCode::Internal),
                },
                watermark,
                frame_id.event_watermark,
            )?;
        } else if frame_id.event_watermark != watermark {
            return Err(ErrorCode::SnapshotRequired);
        }
        sequence = frame.sequence;
        watermark = frame_id.event_watermark;
        result.push(body);
    }
    if sequence != id.sequence || watermark != id.event_watermark {
        return Err(ErrorCode::SnapshotRequired);
    }
    Ok(result)
}

pub fn event_deadline(record: &Record, id: &YsmIdentity) -> Result<u64> {
    let snapshot = materialized(record)?;
    let latest = snapshot.identity.ok_or(ErrorCode::Internal)?;
    if !record.active
        || latest.publisher_session != id.publisher_session
        || latest.generation != id.generation
    {
        return Err(ErrorCode::SnapshotRequired);
    }
    record
        .tail
        .iter()
        .find(|frame| frame.sequence == id.sequence && frame.events > 0)
        .map(|frame| frame.received_at.saturating_add(EVENT_TTL))
        .ok_or(ErrorCode::SnapshotRequired)
}

/// Reduce one authenticated M7 application message as a single durable commit.
/// Its source sequence is distinct from the canonical typed message sequence:
/// a combined state/event input may produce two ordered typed messages.
pub fn publish_legacy(
    record: &mut Record,
    owner: Uuid,
    equipped: &EquippedState,
    scope_key: &[u8],
    frame: &legacy::Frame,
    fingerprint: &[u8],
    now: u64,
) -> Result<Vec<envelope::Body>> {
    let source = &frame.identity;
    validate_identity(owner, equipped, source)?;
    let previous = if record.materialized.is_empty() {
        None
    } else {
        Some(materialized(record)?)
    };
    let old = previous.as_ref().and_then(|value| value.identity.as_ref());
    let same_context = record.active
        && record.equipment_revision == equipped.revision
        && record.publisher_key == scope_key
        && old.is_some_and(|old| {
            old.publisher_session == source.publisher_session && old.generation == source.generation
        });
    let previous_watermark = if same_context {
        let origin = record
            .legacy_origin
            .as_ref()
            .filter(|origin| {
                origin.scope_key == scope_key
                    && origin.publisher == source.publisher_session
                    && origin.generation == source.generation
            })
            .ok_or(ErrorCode::Conflict)?;
        if source.sequence == origin.sequence && fingerprint == origin.fingerprint {
            return Ok(Vec::new());
        }
        if source.sequence <= origin.sequence {
            return Err(ErrorCode::Conflict);
        }
        if !frame.snapshot && source.base_sequence != origin.sequence as i64 {
            return Err(ErrorCode::SnapshotRequired);
        }
        old.ok_or(ErrorCode::SnapshotRequired)?.event_watermark
    } else {
        if !frame.snapshot {
            return Err(ErrorCode::SnapshotRequired);
        }
        source.event_watermark // A fresh baseline never replays its historical effects.
    };
    if source.event_watermark < previous_watermark {
        return Err(ErrorCode::Conflict);
    }
    let events: Vec<_> = frame
        .events
        .iter()
        .filter(|event| event.sequence > previous_watermark)
        .cloned()
        .collect();
    if source.event_watermark > previous_watermark {
        validate_events(&events, previous_watermark, source.event_watermark)?;
    } else if !events.is_empty() {
        return Err(ErrorCode::Conflict);
    }
    let mut canonical = source.clone();
    canonical.sequence = if same_context {
        old.ok_or(ErrorCode::Internal)?
            .sequence
            .checked_add(1)
            .ok_or(ErrorCode::LimitExceeded)?
    } else {
        0
    };
    canonical.base_sequence = if frame.snapshot {
        -1
    } else {
        canonical.sequence as i64 - 1
    };
    canonical.event_watermark = previous_watermark;
    let state = if frame.snapshot {
        envelope::Body::YsmSnapshot(YsmSnapshot {
            identity: Some(canonical.clone()),
            state: Some(frame.state.clone()),
        })
    } else {
        envelope::Body::YsmDelta(YsmDelta {
            identity: Some(canonical.clone()),
            state: Some(frame.state.clone()),
        })
    };
    publish(record, owner, equipped, scope_key, &state, now)?;
    let mut output = vec![state];
    if !events.is_empty() {
        canonical.base_sequence = canonical.sequence as i64;
        canonical.sequence = canonical
            .sequence
            .checked_add(1)
            .ok_or(ErrorCode::LimitExceeded)?;
        canonical.event_watermark = source.event_watermark;
        let event = envelope::Body::YsmEvent(YsmEvent {
            identity: Some(canonical),
            events,
        });
        publish(record, owner, equipped, scope_key, &event, now)?;
        output.push(event);
    }
    record.legacy_origin = Some(LegacyOrigin {
        scope_key: scope_key.to_vec(),
        publisher: source.publisher_session.clone(),
        generation: source.generation,
        sequence: source.sequence,
        fingerprint: fingerprint.to_vec(),
    });
    Ok(output)
}
