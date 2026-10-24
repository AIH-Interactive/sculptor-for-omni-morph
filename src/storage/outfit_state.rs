//! Bounded instance state, independent of equipment revisions and catalog membership.
use super::*;
use crate::service::{library::sql_error, outfit::ResourceRef, outfit_limits::OutfitLimits};
use base64::{engine::general_purpose::STANDARD, Engine};
use rusqlite::{params, OptionalExtension};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    pub generation: Uuid,
    pub sequence: u64,
    pub controls: BTreeMap<String, serde_json::Value>,
    pub lua_state: BTreeMap<String, serde_json::Value>,
    pub animation: Option<String>,
    pub template_version: String,
    pub template_signature: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstanceState {
    pub outfit_revision: u64,
    pub instance_id: Uuid,
    pub resource: ResourceRef,
    pub revision: u64,
    pub state: State,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    outfit_revision: u64,
    instances: BTreeMap<Uuid, InstanceState>,
}

fn key(owner: Uuid) -> String {
    format!("outfit_instance_state:{owner}")
}

fn read(database: &rusqlite::Connection, owner: Uuid, revision: u64) -> Result<Saved> {
    let json: Option<String> = database
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            [key(owner)],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let saved: Option<Saved> = json
        .map(|json| serde_json::from_str(&json).map_err(|_| ErrorCode::Internal))
        .transpose()?;
    Ok(saved
        .filter(|saved| saved.outfit_revision == revision)
        .unwrap_or(Saved {
            outfit_revision: revision,
            instances: BTreeMap::new(),
        }))
}

fn validate(
    state: &State,
    owner: Uuid,
    descriptor: &AvatarDescriptor,
    limits: &OutfitLimits,
) -> Result<()> {
    if state.generation.is_nil() || state.sequence == 0 || state.sequence > i64::MAX as u64 {
        return Err(ErrorCode::InvalidMessage);
    }
    if state.template_version.len() > 64
        || !((state.template_version.is_empty() && state.template_signature.is_empty())
            || (!state.template_version.is_empty()
                && state.template_signature.len() == 64
                && state
                    .template_signature
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))))
    {
        return Err(ErrorCode::InvalidMessage);
    }
    for (values, maximum) in [
        (&state.controls, limits.max_controls),
        (&state.lua_state, limits.max_lua_state_entries),
    ] {
        if values.len() > maximum {
            return Err(ErrorCode::LimitExceeded);
        }
        for (id, value) in values {
            if id.trim().is_empty() || id.len() > 128 {
                return Err(ErrorCode::InvalidMessage);
            }
            let valid = match value {
                serde_json::Value::Bool(_) => true,
                serde_json::Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
                serde_json::Value::String(text) => text.len() <= limits.max_state_string_bytes,
                _ => false,
            };
            if !valid {
                return Err(ErrorCode::InvalidMessage);
            }
        }
    }
    if let Some(encoded) = &state.animation {
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| ErrorCode::InvalidMessage)?;
        let frame = crate::service::ysm::legacy::decode(&bytes, descriptor.revision)?;
        if !matches!(descriptor.model_type, 2 | 3)
            || !frame.snapshot
            || frame.identity.owner != owner.to_string()
            || frame.identity.publisher_session != state.generation.to_string()
            || frame.identity.sequence != state.sequence
            || frame.identity.content_sha256 != descriptor.content_sha256
            || frame.identity.schema_sha256 != descriptor.schema_sha256
        {
            return Err(ErrorCode::Conflict);
        }
        crate::service::ysm::validate_state(&frame.state, true)?;
    }
    if serde_json::to_vec(state)
        .map_err(|_| ErrorCode::Internal)?
        .len()
        > 65_536
    {
        return Err(ErrorCode::LimitExceeded);
    }
    Ok(())
}

impl Storage {
    /// Reuse the current projection inside the same database snapshot as the state read/write.
    pub(crate) fn outfit_instance_state(
        &self,
        owner: Uuid,
        viewer: Uuid,
        origin: &str,
        instance: Uuid,
        limits: &OutfitLimits,
        update: Option<&InstanceState>,
    ) -> Result<InstanceState> {
        self.library_transaction(|database| {
            let projected =
                outfit_projection::project(database, owner, viewer, origin, limits, true)?;
            let configuration = projected.configuration.ok_or(ErrorCode::NotFound)?;
            let declaration = configuration
                .instances
                .iter()
                .find(|value| value.instance_id == instance && value.enabled)
                .ok_or(ErrorCode::NotFound)?;
            let source = projected
                .resources
                .iter()
                .find(|source| source.reference == declaration.resource)
                .ok_or(ErrorCode::NotFound)?;
            let mut saved = read(database, owner, configuration.revision)?;
            let previous = saved
                .instances
                .get(&instance)
                .cloned()
                .unwrap_or(InstanceState {
                    outfit_revision: configuration.revision,
                    instance_id: instance,
                    resource: declaration.resource.clone(),
                    revision: 0,
                    state: State {
                        generation: Uuid::nil(),
                        sequence: 0,
                        controls: declaration.controls.clone(),
                        lua_state: declaration.lua_state.clone(),
                        animation: None,
                        template_version: String::new(),
                        template_signature: String::new(),
                    },
                });
            let Some(input) = update else {
                return Ok(previous);
            };
            if owner != viewer {
                return Err(ErrorCode::Forbidden);
            }
            if input.outfit_revision != configuration.revision
                || input.instance_id != instance
                || input.resource != declaration.resource
            {
                return Err(ErrorCode::Conflict);
            }
            if input.revision.checked_add(1) == Some(previous.revision)
                && input.state == previous.state
            {
                return Ok(previous);
            }
            if input.revision != previous.revision {
                return Err(ErrorCode::Conflict);
            }
            validate(&input.state, owner, &source.descriptor, limits)?;
            if input.state.generation == previous.state.generation {
                if input.state.sequence
                    != previous
                        .state
                        .sequence
                        .checked_add(1)
                        .ok_or(ErrorCode::LimitExceeded)?
                {
                    return Err(ErrorCode::Conflict);
                }
            } else if input.state.sequence != 1 {
                return Err(ErrorCode::Conflict);
            }
            let mut committed = input.clone();
            committed.revision = committed
                .revision
                .checked_add(1)
                .filter(|value| *value <= i64::MAX as u64)
                .ok_or(ErrorCode::LimitExceeded)?;
            saved.instances.retain(|id, _| {
                configuration
                    .instances
                    .iter()
                    .any(|value| value.instance_id == *id)
            });
            saved.instances.insert(instance, committed.clone());
            database
                .execute(
                    "INSERT INTO metadata(key, value) VALUES (?1, ?2)
                ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    params![
                        key(owner),
                        serde_json::to_string(&saved).map_err(|_| ErrorCode::Internal)?
                    ],
                )
                .map_err(sql_error)?;
            Ok(committed)
        })
    }
}
