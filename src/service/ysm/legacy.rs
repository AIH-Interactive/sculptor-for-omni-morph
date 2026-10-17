//! Existing Java M7 wire format. Protobuf frames never enter this codec.
use super::*;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub const PING_ID: u32 = 0x8000_0000;
const FRAGMENT_BYTES: usize = 455;
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub identity: YsmIdentity,
    pub snapshot: bool,
    pub state: YsmState,
    pub events: Vec<Event>,
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(ErrorCode::InvalidMessage)?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(ErrorCode::InvalidMessage)?;
        self.position = end;
        Ok(bytes)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| ErrorCode::InvalidMessage)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }
    fn short(&mut self) -> Result<u32> {
        Ok(u16::from_be_bytes(self.array()?) as u32)
    }
    fn integer(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }
    fn long(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.array()?))
    }
    fn clock(&mut self) -> Result<u64> {
        u64::try_from(self.long()?).map_err(|_| ErrorCode::InvalidMessage)
    }
    fn float(&mut self) -> Result<f32> {
        Ok(f32::from_be_bytes(self.array()?))
    }
    fn boolean(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ErrorCode::InvalidMessage),
        }
    }
    fn section<T>(
        &mut self,
        mask: u8,
        bit: u8,
        width: usize,
        mut read: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Option<Vec<T>>> {
        if mask & bit == 0 {
            return Ok(None);
        }
        let count = self.short()? as usize;
        if count > 512 || count > (self.bytes.len() - self.position) / width {
            return Err(ErrorCode::LimitExceeded);
        }
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(read(self)?);
        }
        Ok(Some(values))
    }
}

pub fn decode(bytes: &[u8], asset_revision: u64) -> Result<Frame> {
    if bytes.len() > MAX_STATE_BYTES {
        return Err(ErrorCode::LimitExceeded);
    }
    let mut input = Reader { bytes, position: 0 };
    if input.take(4)? != b"YSMS" || input.byte()? != 1 {
        return Err(ErrorCode::UnsupportedVersion);
    }
    let owner = Uuid::from_bytes(input.array()?).to_string();
    let publisher_session = Uuid::from_bytes(input.array()?).to_string();
    let content_sha256 = input.take(32)?.to_vec();
    let schema_sha256 = input.take(32)?.to_vec();
    let generation = input.clock()?;
    let sequence = input.clock()?;
    let base_sequence = input.long()?;
    let owner_tick = input.clock()?;
    let snapshot = input.boolean()?;
    let event_watermark = input.clock()?;
    let mask = input.byte()?;
    if mask & !31 != 0
        || (snapshot && base_sequence != -1)
        || (!snapshot && (base_sequence < 0 || base_sequence as u64 >= sequence))
    {
        return Err(ErrorCode::InvalidMessage);
    }
    let texture = if mask & 1 != 0 {
        Some(input.integer()?)
    } else {
        None
    };
    let controls = input
        .section(mask, 2, 11, |input| {
            Ok(Control {
                index: input.short()?,
                r#type: input.byte()? as i32 + 1,
                value: f64::from_be_bytes(input.array()?),
            })
        })?
        .map(|values| Controls { values });
    let actions = input
        .section(mask, 4, 12, |input| {
            Ok(Action {
                index: input.short()?,
                active: input.boolean()?,
                time: input.float()?,
                speed: input.float()?,
                r#loop: input.boolean()?,
            })
        })?
        .map(|values| Actions { values });
    let controllers = input
        .section(mask, 8, 8, |input| {
            Ok(Controller {
                index: input.short()?,
                state: input.short()?,
                time: input.float()?,
            })
        })?
        .map(|values| Controllers { values });
    let playbacks = input
        .section(mask, 16, 30, |input| {
            Ok(Playback {
                slot: input.short()?,
                clip: input.short()?,
                controller: input.integer()?,
                time: input.float()?,
                speed: input.float()?,
                weight: input.float()?,
                target_weight: input.float()?,
                loop_mode: input.byte()? as u32,
                fade_speed: input.float()?,
                flags: input.byte()? as u32,
            })
        })?
        .map(|values| Playbacks { values });
    let state = YsmState {
        texture,
        controls,
        actions,
        controllers,
        playbacks,
    };
    validate_state(&state, snapshot)?;
    let count = input.byte()? as usize;
    if count > 64 || count > (bytes.len() - input.position) / 10 {
        return Err(ErrorCode::LimitExceeded);
    }
    let mut events = Vec::with_capacity(count);
    let mut previous = 0;
    for _ in 0..count {
        let sequence = input.clock()?;
        let kind = input.byte()? as i32 + 1;
        let count = input.byte()? as usize;
        if sequence <= previous
            || sequence > event_watermark
            || !(1..=4).contains(&kind)
            || count > 16
            || count > (bytes.len() - input.position) / 4
        {
            return Err(ErrorCode::InvalidMessage);
        }
        let mut arguments = Vec::with_capacity(count);
        for _ in 0..count {
            let value = input.float()?;
            if !value.is_finite() {
                return Err(ErrorCode::InvalidMessage);
            }
            arguments.push(value);
        }
        events.push(Event {
            sequence,
            kind,
            arguments,
        });
        previous = sequence;
    }
    if input.position != bytes.len() {
        return Err(ErrorCode::InvalidMessage);
    }
    Ok(Frame {
        identity: YsmIdentity {
            owner,
            asset_revision,
            publisher_session,
            generation,
            sequence,
            base_sequence,
            owner_tick,
            content_sha256,
            schema_sha256,
            event_watermark,
        },
        snapshot,
        state,
        events,
    })
}

fn write_section<T>(
    output: &mut Vec<u8>,
    values: Option<&[T]>,
    mut write: impl FnMut(&mut Vec<u8>, &T),
) {
    if let Some(values) = values {
        output.extend_from_slice(&(values.len() as u16).to_be_bytes());
        for value in values {
            write(output, value);
        }
    }
}

pub fn encode(frame: &Frame) -> Result<Vec<u8>> {
    validate_state(&frame.state, frame.snapshot)?;
    let id = &frame.identity;
    if id.content_sha256.len() != 32 || id.schema_sha256.len() != 32 || frame.events.len() > 64 {
        return Err(ErrorCode::InvalidMessage);
    }
    let mut output = b"YSMS\x01".to_vec();
    for value in [&id.owner, &id.publisher_session] {
        output.extend_from_slice(
            Uuid::parse_str(value)
                .map_err(|_| ErrorCode::InvalidMessage)?
                .as_bytes(),
        );
    }
    output.extend_from_slice(&id.content_sha256);
    output.extend_from_slice(&id.schema_sha256);
    for value in [id.generation, id.sequence] {
        output.extend_from_slice(&value.to_be_bytes());
    }
    output.extend_from_slice(&id.base_sequence.to_be_bytes());
    output.extend_from_slice(&id.owner_tick.to_be_bytes());
    output.push(u8::from(frame.snapshot));
    output.extend_from_slice(&id.event_watermark.to_be_bytes());
    let state = &frame.state;
    let mask = u8::from(state.texture.is_some())
        | u8::from(state.controls.is_some()) << 1
        | u8::from(state.actions.is_some()) << 2
        | u8::from(state.controllers.is_some()) << 3
        | u8::from(state.playbacks.is_some()) << 4;
    output.push(mask);
    if let Some(texture) = state.texture {
        output.extend_from_slice(&texture.to_be_bytes());
    }
    write_section(
        &mut output,
        state
            .controls
            .as_ref()
            .map(|values| values.values.as_slice()),
        |out, value| {
            out.extend_from_slice(&(value.index as u16).to_be_bytes());
            out.push((value.r#type - 1) as u8);
            out.extend_from_slice(&value.value.to_be_bytes());
        },
    );
    write_section(
        &mut output,
        state
            .actions
            .as_ref()
            .map(|values| values.values.as_slice()),
        |out, value| {
            out.extend_from_slice(&(value.index as u16).to_be_bytes());
            out.push(u8::from(value.active));
            out.extend_from_slice(&value.time.to_be_bytes());
            out.extend_from_slice(&value.speed.to_be_bytes());
            out.push(u8::from(value.r#loop));
        },
    );
    write_section(
        &mut output,
        state
            .controllers
            .as_ref()
            .map(|values| values.values.as_slice()),
        |out, value| {
            out.extend_from_slice(&(value.index as u16).to_be_bytes());
            out.extend_from_slice(&(value.state as u16).to_be_bytes());
            out.extend_from_slice(&value.time.to_be_bytes());
        },
    );
    write_section(
        &mut output,
        state
            .playbacks
            .as_ref()
            .map(|values| values.values.as_slice()),
        |out, value| {
            out.extend_from_slice(&(value.slot as u16).to_be_bytes());
            out.extend_from_slice(&(value.clip as u16).to_be_bytes());
            out.extend_from_slice(&value.controller.to_be_bytes());
            for number in [value.time, value.speed, value.weight, value.target_weight] {
                out.extend_from_slice(&number.to_be_bytes());
            }
            out.push(value.loop_mode as u8);
            out.extend_from_slice(&value.fade_speed.to_be_bytes());
            out.push(value.flags as u8);
        },
    );
    output.push(frame.events.len() as u8);
    for event in &frame.events {
        if event.arguments.len() > 16 || !(1..=4).contains(&event.kind) {
            return Err(ErrorCode::InvalidMessage);
        }
        output.extend_from_slice(&event.sequence.to_be_bytes());
        output.push((event.kind - 1) as u8);
        output.push(event.arguments.len() as u8);
        for value in &event.arguments {
            output.extend_from_slice(&value.to_be_bytes());
        }
    }
    if output.len() > MAX_STATE_BYTES {
        return Err(ErrorCode::LimitExceeded);
    }
    // Match the Java constructor's clock, enum and finite-value validation too.
    decode(&output, id.asset_revision)?;
    Ok(output)
}

pub fn encode_body(body: &envelope::Body) -> Result<Option<Vec<u8>>> {
    let (snapshot, state, events) = match body {
        envelope::Body::YsmSnapshot(value) => (
            true,
            value.state.clone().ok_or(ErrorCode::InvalidMessage)?,
            Vec::new(),
        ),
        envelope::Body::YsmDelta(value) => (
            false,
            value.state.clone().ok_or(ErrorCode::InvalidMessage)?,
            Vec::new(),
        ),
        envelope::Body::YsmEvent(value) => (false, YsmState::default(), value.events.clone()),
        // Existing M7 has no request/error control frames. Its periodic full
        // snapshots remain the recovery source for an unchanged legacy owner.
        _ => return Ok(None),
    };
    encode(&Frame {
        identity: identity(body)?.clone(),
        snapshot,
        state,
        events,
    })
    .map(Some)
}

pub fn recognizes(function_id: u32, bytes: &[u8]) -> bool {
    function_id == PING_ID && bytes.starts_with(b"YSMP")
}

pub fn fragment(message_id: u64, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    if bytes.is_empty() || bytes.len() > MAX_STATE_BYTES {
        return Err(ErrorCode::LimitExceeded);
    }
    let count = bytes.len().div_ceil(FRAGMENT_BYTES);
    let crc = crc32fast::hash(bytes);
    let mut result = Vec::with_capacity(count);
    for (index, chunk) in bytes.chunks(FRAGMENT_BYTES).enumerate() {
        let mut packet = b"YSMP\x01".to_vec();
        packet.extend_from_slice(&message_id.to_be_bytes());
        packet.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        packet.extend_from_slice(&(index as u16).to_be_bytes());
        packet.extend_from_slice(&(count as u16).to_be_bytes());
        packet.extend_from_slice(&crc.to_be_bytes());
        packet.extend_from_slice(chunk);
        result.push(packet);
    }
    Ok(result)
}

struct Assembly {
    bytes: Vec<u8>,
    received: Vec<bool>,
    checksum: u32,
    started: Instant,
}

#[derive(Default)]
pub struct Reassembly {
    incoming: HashMap<(Uuid, u64), Assembly>,
}

impl Reassembly {
    pub fn accept(&mut self, owner: Uuid, packet: &[u8]) -> Result<Option<Vec<u8>>> {
        self.incoming
            .retain(|_, value| value.started.elapsed() < Duration::from_secs(60));
        if !(25..=480).contains(&packet.len()) {
            return Err(ErrorCode::InvalidMessage);
        }
        let mut input = Reader {
            bytes: packet,
            position: 0,
        };
        if input.take(4)? != b"YSMP" || input.byte()? != 1 {
            return Err(ErrorCode::UnsupportedVersion);
        }
        let message_id = u64::from_be_bytes(input.array()?);
        let length = u32::from_be_bytes(input.array()?) as usize;
        let index = input.short()? as usize;
        let count = input.short()? as usize;
        let checksum = u32::from_be_bytes(input.array()?);
        if length == 0
            || length > MAX_STATE_BYTES
            || count != length.div_ceil(FRAGMENT_BYTES)
            || index >= count
        {
            return Err(ErrorCode::InvalidMessage);
        }
        let offset = index * FRAGMENT_BYTES;
        let length_here = FRAGMENT_BYTES.min(length - offset);
        if packet.len() != 25 + length_here {
            return Err(ErrorCode::InvalidMessage);
        }
        let key = (owner, message_id);
        if !self.incoming.contains_key(&key) {
            if self.incoming.len() >= 16
                || self
                    .incoming
                    .keys()
                    .filter(|(candidate, _)| *candidate == owner)
                    .count()
                    >= 2
            {
                return Err(ErrorCode::LimitExceeded);
            }
            self.incoming.insert(
                key,
                Assembly {
                    bytes: vec![0; length],
                    received: vec![false; count],
                    checksum,
                    started: Instant::now(),
                },
            );
        }
        let assembly = self.incoming.get_mut(&key).ok_or(ErrorCode::Internal)?;
        if assembly.bytes.len() != length
            || assembly.checksum != checksum
            || (assembly.received[index]
                && assembly.bytes[offset..offset + length_here] != packet[25..])
        {
            self.incoming.remove(&key);
            return Err(ErrorCode::Conflict);
        }
        assembly.bytes[offset..offset + length_here].copy_from_slice(&packet[25..]);
        assembly.received[index] = true;
        if !assembly.received.iter().all(|value| *value) {
            return Ok(None);
        }
        let assembly = self.incoming.remove(&key).ok_or(ErrorCode::Internal)?;
        if crc32fast::hash(&assembly.bytes) != checksum {
            return Err(ErrorCode::InvalidMessage);
        }
        Ok(Some(assembly.bytes))
    }
}
