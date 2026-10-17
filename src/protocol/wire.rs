use super::v1::{Envelope, ErrorCode};
use prost::Message;
use prost_types::{
    field_descriptor_proto::{Label, Type},
    DescriptorProto, FileDescriptorSet,
};
use std::{collections::HashMap, sync::OnceLock};

pub const MAX_ENVELOPE_BYTES: usize = 65_536;
pub const MAX_STATE_BYTES: usize = 32_768;
pub const MAX_ENTRIES: usize = 512;
pub const MAX_DEPTH: usize = 16;

struct Schema {
    messages: HashMap<String, DescriptorProto>,
    enums: HashMap<String, Vec<i32>>,
}

fn schema() -> &'static Schema {
    static SCHEMA: OnceLock<Schema> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let descriptors = FileDescriptorSet::decode(
            include_bytes!(concat!(env!("OUT_DIR"), "/cloud-descriptor.bin")).as_slice(),
        )
        .expect("Build-generated descriptor must be valid");
        let mut schema = Schema {
            messages: HashMap::new(),
            enums: HashMap::new(),
        };
        for file in descriptors.file {
            let prefix = format!(".{}", file.package());
            for message in file.message_type {
                schema
                    .messages
                    .insert(format!("{prefix}.{}", message.name()), message);
            }
            for enumeration in file.enum_type {
                schema.enums.insert(
                    format!("{prefix}.{}", enumeration.name()),
                    enumeration
                        .value
                        .iter()
                        .map(|value| value.number())
                        .collect(),
                );
            }
        }
        schema
    })
}

pub fn decode(bytes: &[u8]) -> Result<Envelope, ErrorCode> {
    if bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(ErrorCode::LimitExceeded);
    }
    inspect(bytes, ".omnimorph.cloud.v1.Envelope", 0)?;
    let envelope = Envelope::decode(bytes).map_err(|_| ErrorCode::InvalidMessage)?;
    if envelope.major != 1 {
        return Err(ErrorCode::UnsupportedVersion);
    }
    if envelope.body.is_none() {
        return Err(ErrorCode::InvalidMessage);
    }
    Ok(envelope)
}

fn read_varint(bytes: &[u8], position: &mut usize) -> Result<u64, ErrorCode> {
    let mut result = 0;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*position).ok_or(ErrorCode::InvalidMessage)?;
        *position += 1;
        if shift == 63 && byte > 1 {
            return Err(ErrorCode::InvalidMessage);
        }
        result |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(result);
        }
    }
    Err(ErrorCode::InvalidMessage)
}

fn take<'a>(bytes: &'a [u8], position: &mut usize, size: usize) -> Result<&'a [u8], ErrorCode> {
    let end = position
        .checked_add(size)
        .ok_or(ErrorCode::InvalidMessage)?;
    let result = bytes.get(*position..end).ok_or(ErrorCode::InvalidMessage)?;
    *position = end;
    Ok(result)
}

fn inspect(bytes: &[u8], name: &str, depth: usize) -> Result<(), ErrorCode> {
    if depth > MAX_DEPTH {
        return Err(ErrorCode::LimitExceeded);
    }
    if name.ends_with(".YsmSnapshot") || name.ends_with(".YsmDelta") || name.ends_with(".YsmEvent")
    {
        if bytes.len() > MAX_STATE_BYTES {
            return Err(ErrorCode::LimitExceeded);
        }
    }
    let schema = schema();
    let descriptor = schema.messages.get(name).ok_or(ErrorCode::InvalidMessage)?;
    let mut counts = HashMap::<i32, usize>::new();
    let mut oneofs = HashMap::<i32, usize>::new();
    let mut position = 0;
    while position < bytes.len() {
        let tag = read_varint(bytes, &mut position)?;
        let number = i32::try_from(tag >> 3).map_err(|_| ErrorCode::InvalidMessage)?;
        if number == 0 || number > 0x1fff_ffff {
            return Err(ErrorCode::InvalidMessage);
        }
        let wire = tag & 7;
        let (value, payload) = match wire {
            0 => (read_varint(bytes, &mut position)?, &[][..]),
            1 => (0, take(bytes, &mut position, 8)?),
            2 => {
                let size = usize::try_from(read_varint(bytes, &mut position)?)
                    .map_err(|_| ErrorCode::InvalidMessage)?;
                (0, take(bytes, &mut position, size)?)
            }
            5 => (0, take(bytes, &mut position, 4)?),
            _ => return Err(ErrorCode::InvalidMessage),
        };
        let Some(field) = descriptor
            .field
            .iter()
            .find(|field| field.number() == number)
        else {
            continue;
        };
        let kind = field.r#type();
        let repeated = field.label() == Label::Repeated;
        let expected_wire = match kind {
            Type::Double | Type::Fixed64 | Type::Sfixed64 => 1,
            Type::String | Type::Bytes | Type::Message => 2,
            Type::Float | Type::Fixed32 | Type::Sfixed32 => 5,
            _ => 0,
        };
        let mut count = 1;
        if wire == 2 && repeated && expected_wire != 2 {
            count = 0;
            let mut packed_position = 0;
            while packed_position < payload.len() {
                if expected_wire == 0 {
                    let value = read_varint(payload, &mut packed_position)?;
                    inspect_scalar(schema, kind, field.type_name(), value, &[])?;
                } else {
                    let width = if expected_wire == 1 { 8 } else { 4 };
                    let scalar = take(payload, &mut packed_position, width)?;
                    inspect_scalar(schema, kind, field.type_name(), 0, scalar)?;
                }
                count += 1;
            }
        } else {
            if wire != expected_wire {
                return Err(ErrorCode::InvalidMessage);
            }
            match kind {
                Type::Message => inspect(payload, field.type_name(), depth + 1)?,
                Type::String => {
                    std::str::from_utf8(payload).map_err(|_| ErrorCode::InvalidMessage)?;
                    if payload.len() > 4096 {
                        return Err(ErrorCode::LimitExceeded);
                    }
                }
                _ => inspect_scalar(schema, kind, field.type_name(), value, payload)?,
            }
        }
        let seen = counts.entry(number).or_default();
        *seen += count;
        let limit = if !repeated {
            1
        } else if name.ends_with(".Subscribe") || name.ends_with(".Unsubscribe") {
            128
        } else if name.ends_with(".YsmEvent") {
            64
        } else if name.ends_with(".Event") {
            16
        } else {
            MAX_ENTRIES
        };
        if *seen > limit {
            return Err(ErrorCode::LimitExceeded);
        }
        if let Some(oneof) = field.oneof_index {
            let seen = oneofs.entry(oneof).or_default();
            *seen += 1;
            if *seen > 1 {
                return Err(ErrorCode::InvalidMessage);
            }
        }
    }
    Ok(())
}

fn inspect_scalar(
    schema: &Schema,
    kind: Type,
    type_name: &str,
    value: u64,
    payload: &[u8],
) -> Result<(), ErrorCode> {
    match kind {
        Type::Bool if value > 1 => return Err(ErrorCode::InvalidMessage),
        Type::Enum => {
            let values = schema
                .enums
                .get(type_name)
                .ok_or(ErrorCode::InvalidMessage)?;
            let number = value as i32;
            if value != number as i64 as u64 || !values.contains(&number) {
                return Err(ErrorCode::InvalidMessage);
            }
        }
        Type::Uint32 | Type::Sint32 if value > u32::MAX as u64 => {
            return Err(ErrorCode::InvalidMessage);
        }
        Type::Float
            if !f32::from_le_bytes(payload.try_into().map_err(|_| ErrorCode::InvalidMessage)?)
                .is_finite() =>
        {
            return Err(ErrorCode::InvalidMessage);
        }
        Type::Double
            if !f64::from_le_bytes(payload.try_into().map_err(|_| ErrorCode::InvalidMessage)?)
                .is_finite() =>
        {
            return Err(ErrorCode::InvalidMessage);
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::v1::{envelope::Body, Hello};

    #[test]
    fn scalar_validation_rejects_wrapping_values_and_nonfinite_numbers() {
        assert_eq!(
            inspect_scalar(
                schema(),
                Type::Enum,
                ".omnimorph.cloud.v1.Capability",
                (1u64 << 32) + 1,
                &[]
            ),
            Err(ErrorCode::InvalidMessage)
        );
        assert_eq!(
            inspect_scalar(schema(), Type::Uint32, "", 1u64 << 32, &[]),
            Err(ErrorCode::InvalidMessage)
        );
        assert_eq!(
            inspect_scalar(schema(), Type::Sint32, "", 1u64 << 32, &[]),
            Err(ErrorCode::InvalidMessage)
        );
        assert_eq!(
            inspect_scalar(schema(), Type::Float, "", 0, &f32::NAN.to_le_bytes()),
            Err(ErrorCode::InvalidMessage)
        );
        assert_eq!(
            inspect_scalar(schema(), Type::Double, "", 0, &f64::INFINITY.to_le_bytes()),
            Err(ErrorCode::InvalidMessage)
        );
    }

    #[test]
    fn wire_budgets_precede_generated_decoding() {
        let valid = Envelope {
            major: 1,
            body: Some(Body::Hello(Hello {
                supported_majors: vec![1],
                ..Default::default()
            })),
            ..Default::default()
        };
        let bytes = valid.encode_to_vec();
        assert_eq!(decode(&bytes).unwrap(), valid);
        assert_eq!(
            decode(&vec![0; MAX_ENVELOPE_BYTES + 1]),
            Err(ErrorCode::LimitExceeded)
        );
        assert_eq!(decode(&[0x52, 5, 1]), Err(ErrorCode::InvalidMessage));
        let mut unknown = bytes.clone();
        unknown.extend_from_slice(&[0xa0, 6, 7]);
        assert!(decode(&unknown).is_ok());
        let mut duplicate = bytes.clone();
        duplicate.extend_from_slice(&bytes);
        assert!(decode(&duplicate).is_err());
        let oversized = Envelope {
            body: Some(Body::Hello(Hello {
                supported_majors: vec![1; 513],
                ..Default::default()
            })),
            ..valid
        };
        assert_eq!(
            decode(&oversized.encode_to_vec()),
            Err(ErrorCode::LimitExceeded)
        );
    }
}
