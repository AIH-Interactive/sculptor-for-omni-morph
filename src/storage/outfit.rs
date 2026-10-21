//! Owner-only configuration and presets. Publishing/equipping belongs to later stages.
use super::{library, now_seconds, sqlite::sql_error, ErrorCode, Result, Storage};
use crate::service::outfit::OutfitConfig;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresetInput {
    pub operation_id: Uuid,
    pub preset_id: Uuid,
    pub expected_revision: u64,
    pub name: String,
    pub configuration: OutfitConfig,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct Preset {
    pub preset_id: Uuid,
    pub revision: u64,
    pub name: String,
    pub configuration: OutfitConfig,
}

fn read_preset(database: &rusqlite::Connection, owner: Uuid, id: Uuid) -> Result<Preset> {
    let (revision, name, bytes): (i64, String, String) = database
        .query_row(
            "SELECT revision, name, configuration_json FROM outfit_presets
         WHERE owner_uuid = ?1 AND preset_id = ?2",
            params![owner.to_string(), id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql_error)?
        .ok_or(ErrorCode::NotFound)?;
    let configuration = serde_json::from_str(&bytes).map_err(|_| ErrorCode::Internal)?;
    Ok(Preset {
        preset_id: id,
        revision: u64::try_from(revision).map_err(|_| ErrorCode::Internal)?,
        name,
        configuration,
    })
}

impl Storage {
    pub fn outfit_current(&self, owner: Uuid) -> Result<OutfitConfig> {
        self.library_read(|database| {
            let json: Option<String> = database
                .query_row(
                    "SELECT configuration_json FROM outfit_configurations WHERE owner_uuid = ?1",
                    [owner.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            json.map(|json| serde_json::from_str(&json).map_err(|_| ErrorCode::Internal))
                .unwrap_or_else(|| Ok(OutfitConfig::default()))
        })
    }

    pub fn outfit_preset(&self, owner: Uuid, id: Uuid) -> Result<Preset> {
        self.library_read(|database| read_preset(database, owner, id))
    }

    pub fn outfit_presets(&self, owner: Uuid) -> Result<Vec<serde_json::Value>> {
        self.library_read(|database| {
            let mut statement = database.prepare(
                "SELECT preset_id, revision, name FROM outfit_presets WHERE owner_uuid = ?1 ORDER BY preset_id",
            ).map_err(sql_error)?;
            let rows = statement.query_map([owner.to_string()], |row| {
                Ok(serde_json::json!({"preset_id": row.get::<_, String>(0)?,
                    "revision": row.get::<_, i64>(1)?, "name": row.get::<_, String>(2)?}))
            }).map_err(sql_error)?;
            rows.map(|row| row.map_err(sql_error)).collect()
        })
    }

    pub fn outfit_save_preset(
        &self,
        owner: Uuid,
        input: &PresetInput,
        limits: &crate::service::outfit_limits::OutfitLimits,
    ) -> Result<Preset> {
        limits.validate()?;
        input.configuration.validate_with_limits(limits)?;
        crate::service::library::text(&input.name, 96, false)?;
        let fingerprint = super::sha256(
            &serde_json::to_vec(&("outfit-preset", input))
                .map_err(|_| ErrorCode::InvalidMessage)?,
        );
        self.library_transaction(|transaction| {
            let bytes = library::operation(transaction, owner, input.operation_id, &fingerprint, now_seconds(), |transaction| {
                let existing: Option<i64> = transaction.query_row(
                    "SELECT revision FROM outfit_presets WHERE owner_uuid = ?1 AND preset_id = ?2",
                    params![owner.to_string(), input.preset_id.to_string()], |row| row.get(0),
                ).optional().map_err(sql_error)?;
                if existing.unwrap_or(0) != library::sql_integer(input.expected_revision)? {
                    return Err(ErrorCode::Conflict);
                }
                if existing.is_none() {
                    let count: u32 = transaction.query_row(
                        "SELECT count(*) FROM outfit_presets WHERE owner_uuid = ?1",
                        [owner.to_string()], |row| row.get(0),
                    ).map_err(sql_error)?;
                    if count as usize >= limits.max_presets {
                        return Err(ErrorCode::LimitExceeded);
                    }
                }
                let revision = input.expected_revision.checked_add(1).ok_or(ErrorCode::InvalidMessage)?;
                let configuration = serde_json::to_string(&input.configuration).map_err(|_| ErrorCode::Internal)?;
                transaction.execute(
                    "INSERT INTO outfit_presets VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(owner_uuid, preset_id) DO UPDATE SET revision = excluded.revision,
                     name = excluded.name, configuration_json = excluded.configuration_json",
                    params![owner.to_string(), input.preset_id.to_string(), library::sql_integer(revision)?, input.name, configuration],
                ).map_err(sql_error)?;
                transaction.execute(
                    "DELETE FROM outfit_preset_dependencies WHERE owner_uuid = ?1 AND preset_id = ?2",
                    params![owner.to_string(), input.preset_id.to_string()],
                ).map_err(sql_error)?;
                for (index, dependency) in input.configuration.dependencies().into_iter().enumerate() {
                    transaction.execute(
                        "INSERT INTO outfit_preset_dependencies VALUES (?1, ?2, ?3, ?4)",
                        params![owner.to_string(), input.preset_id.to_string(), index as i64,
                            serde_json::to_string(dependency).map_err(|_| ErrorCode::Internal)?],
                    ).map_err(sql_error)?;
                }
                serde_json::to_vec(&read_preset(transaction, owner, input.preset_id)?).map_err(|_| ErrorCode::Internal)
            })?;
            serde_json::from_slice(&bytes).map_err(|_| ErrorCode::Internal)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_scope_cas_idempotence_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let owner = Uuid::from_u128(1);
        let other = Uuid::from_u128(2);
        let mut input = PresetInput {
            operation_id: Uuid::from_u128(3),
            preset_id: Uuid::from_u128(4),
            expected_revision: 0,
            name: "星翼".into(),
            configuration: OutfitConfig::default(),
        };
        let saved = storage
            .outfit_save_preset(
                owner,
                &input,
                &crate::service::outfit_limits::OutfitLimits::default(),
            )
            .unwrap();
        assert_eq!(
            saved,
            storage
                .outfit_save_preset(
                    owner,
                    &input,
                    &crate::service::outfit_limits::OutfitLimits::default()
                )
                .unwrap()
        );
        assert_eq!(
            storage.outfit_preset(other, input.preset_id).unwrap_err(),
            ErrorCode::NotFound
        );
        input.name = "conflicting retry".into();
        assert_eq!(
            storage
                .outfit_save_preset(
                    owner,
                    &input,
                    &crate::service::outfit_limits::OutfitLimits::default()
                )
                .unwrap_err(),
            ErrorCode::Conflict
        );
        input.operation_id = Uuid::from_u128(5);
        assert_eq!(
            storage
                .outfit_save_preset(
                    owner,
                    &input,
                    &crate::service::outfit_limits::OutfitLimits::default()
                )
                .unwrap_err(),
            ErrorCode::Conflict
        );
        drop(storage);
        let storage = Storage::open(directory.path()).unwrap();
        assert_eq!(
            saved,
            storage.outfit_preset(owner, input.preset_id).unwrap()
        );
        assert_eq!(
            storage.outfit_current(owner).unwrap(),
            OutfitConfig::default()
        );
        assert_eq!(storage.outfit_presets(other).unwrap().len(), 0);
    }
}
