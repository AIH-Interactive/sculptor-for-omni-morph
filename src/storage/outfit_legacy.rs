//! Traditional whole-avatar replacement also retires the previous outfit configuration.
use super::*;
use crate::service::{
    outfit::{OutfitConfig, ResourceKind, ResourceRef},
    outfit_limits::OutfitLimits,
};
use rusqlite::{OptionalExtension, Transaction, params};

fn native_reference(
    transaction: &Transaction<'_>,
    owner: Uuid,
    avatar: &AvatarDescriptor,
) -> Result<ResourceRef> {
    let raw_hash = faster_hex::hex_string(&avatar.raw_sha256);
    let format = match avatar.model_type {
        1 => "FIGURA",
        2 => "YSM_PLAIN",
        3 => "YSM_ENCRYPTED",
        _ => return Err(ErrorCode::UnsupportedCapability),
    };
    let prior: Option<String> = transaction
        .query_row(
            "SELECT binding_json FROM outfit_local_resources
         WHERE owner_uuid = ?1 AND model_type = ?2
           AND json_extract(binding_json, '$.resource_kind') = 'AVATAR'
           AND json_extract(binding_json, '$.asset_id') = ?3
           AND json_extract(binding_json, '$.asset_revision') = ?4
           AND json_extract(binding_json, '$.raw_sha256') = ?5
         ORDER BY local_id, source_hash LIMIT 1",
            params![
                owner.to_string(),
                format,
                avatar.asset_id,
                library::sql_integer(avatar.revision)?,
                raw_hash
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite::sql_error)?;
    if let Some(prior) = prior {
        let binding: outfit_resources::LocalBinding =
            serde_json::from_str(&prior).map_err(|_| ErrorCode::Internal)?;
        binding.validate_native_identity(transaction, owner)?;
        return Ok(binding.reference);
    }
    // Old clients have no local UUID or source-tree hash. Persist a deterministic
    // native-revision identity without pretending to know the original source tree.
    let identity = serde_json::to_vec(&(
        "legacy-outfit-base",
        owner,
        &avatar.asset_id,
        avatar.revision,
    ))
    .map_err(|_| ErrorCode::Internal)?;
    let local_id = Uuid::from_slice(&sha256(&identity)[..16]).map_err(|_| ErrorCode::Internal)?;
    let reference = ResourceRef::Local {
        local_id,
        content_hash: raw_hash.clone(),
        format: format.into(),
    };
    reference.validate()?;
    let binding = outfit_resources::LocalBinding {
        reference: reference.clone(),
        asset_id: avatar.asset_id.clone(),
        asset_revision: avatar.revision,
        raw_sha256: raw_hash.clone(),
        resource_kind: ResourceKind::Avatar,
        accessory_metadata: None,
    };
    // Legacy writes retain their established owner-storage semantics. This
    // projection grants no observer access; modern Apply still rechecks bans.
    binding.validate_native_identity(transaction, owner)?;
    let encoded = serde_json::to_string(&binding).map_err(|_| ErrorCode::Internal)?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO outfit_local_resources VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                owner.to_string(),
                local_id.to_string(),
                raw_hash,
                format,
                encoded
            ],
        )
        .map_err(sqlite::sql_error)?;
    let saved: String = transaction
        .query_row(
            "SELECT binding_json FROM outfit_local_resources
         WHERE owner_uuid = ?1 AND local_id = ?2 AND source_hash = ?3 AND model_type = ?4",
            params![owner.to_string(), local_id.to_string(), raw_hash, format],
            |row| row.get(0),
        )
        .map_err(sqlite::sql_error)?;
    if saved != encoded {
        return Err(ErrorCode::Conflict);
    }
    Ok(reference)
}

pub(super) fn normalize_native(
    transaction: &Transaction<'_>,
    owner: Uuid,
    avatar: Option<&AvatarDescriptor>,
    limits: &OutfitLimits,
) -> Result<()> {
    let base = avatar
        .map(|avatar| native_reference(transaction, owner, avatar))
        .transpose()?;
    let show_equipment = if let Some(avatar) = avatar {
        transaction
            .query_row(
                "SELECT COALESCE((SELECT show_equipment FROM library_wardrobe_settings
             WHERE owner_uuid = ?1 AND asset_id = ?2 AND asset_revision = ?3), 1)",
                params![
                    owner.to_string(),
                    avatar.asset_id,
                    library::sql_integer(avatar.revision)?
                ],
                |row| row.get(0),
            )
            .map_err(sqlite::sql_error)?
    } else {
        false
    };
    replace_configuration(transaction, owner, base, show_equipment, limits)
}

pub(super) fn replace_configuration(
    transaction: &Transaction<'_>,
    owner: Uuid,
    base: Option<ResourceRef>,
    show_equipment: bool,
    limits: &OutfitLimits,
) -> Result<()> {
    let previous: Option<i64> = transaction
        .query_row(
            "SELECT revision FROM outfit_configurations WHERE owner_uuid = ?1",
            [owner.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite::sql_error)?;
    let revision = previous
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(ErrorCode::Conflict)?;
    let configuration = OutfitConfig {
        revision: u64::try_from(revision).map_err(|_| ErrorCode::Internal)?,
        base,
        show_equipment,
        ..Default::default()
    };
    configuration.validate_with_limits(limits)?;
    transaction
        .execute(
            "INSERT INTO outfit_configurations VALUES (?1, ?2, ?3)
         ON CONFLICT(owner_uuid) DO UPDATE SET revision = excluded.revision,
         configuration_json = excluded.configuration_json",
            params![
                owner.to_string(),
                revision,
                serde_json::to_string(&configuration).map_err(|_| ErrorCode::Internal)?
            ],
        )
        .map_err(sqlite::sql_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_configuration(storage: &Storage, owner: Uuid, revision: u64) -> OutfitConfig {
        let mut configuration: OutfitConfig = serde_json::from_str(include_str!(
            "../../tests/fixtures/outfit-v1/duplicate-instances.json"
        ))
        .unwrap();
        configuration.revision = revision;
        storage
            .library_transaction(|transaction| {
                transaction
                    .execute(
                        "INSERT INTO outfit_configurations VALUES (?1, ?2, ?3)
                 ON CONFLICT(owner_uuid) DO UPDATE SET revision = excluded.revision,
                 configuration_json = excluded.configuration_json",
                        params![
                            owner.to_string(),
                            library::sql_integer(revision)?,
                            serde_json::to_string(&configuration).unwrap()
                        ],
                    )
                    .unwrap();
                Ok(())
            })
            .unwrap();
        configuration
    }

    fn native(storage: &Storage, owner: Uuid, asset: &str) -> AvatarDescriptor {
        let (bytes, content) = crate::service::package::tests::fixture(false, false);
        let operation = crate::service::library::random_id();
        let staged = storage.upload_path(owner, operation.parse().unwrap());
        fs::write(&staged, &bytes).unwrap();
        storage.publish_staged(&staged, &sha256(&bytes)).unwrap();
        storage
            .commit_avatar(
                owner,
                &operation,
                &sha256(asset.as_bytes()),
                AvatarDescriptor {
                    owner: owner.to_string(),
                    asset_id: asset.into(),
                    model_type: 2,
                    distribution_format: 2,
                    raw_sha256: sha256(&bytes),
                    raw_length: bytes.len() as u64,
                    content_sha256: content,
                    ..Default::default()
                },
                10,
            )
            .unwrap()
    }

    fn request(avatar: &AvatarDescriptor, revision: u64) -> Equip {
        Equip {
            operation_id: crate::service::library::random_id(),
            asset_id: avatar.asset_id.clone(),
            asset_revision: avatar.revision,
            expected_revision: revision,
            ..Default::default()
        }
    }

    #[test]
    fn legacy_equip_and_clear_replace_the_whole_configuration_once_per_operation() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let owner = Uuid::from_u128(1);
        let avatar = native(&storage, owner, "native");
        seed_configuration(&storage, owner, 5);
        let equip = request(&avatar, 0);
        let equipped = storage.equip(owner, &equip).unwrap();
        let normalized = storage.outfit_current(owner).unwrap();
        assert_eq!(normalized.revision, 6);
        assert!(normalized.instances.is_empty());
        assert!(normalized.virtual_anchors.is_empty());
        assert!(normalized.display_ordinals.is_empty());
        assert!(normalized.show_equipment);
        storage
            .library_read(|database| {
                let binding = outfit_resources::resolve_local(
                    database,
                    owner,
                    normalized.base.as_ref().unwrap(),
                    ResourceKind::Avatar,
                    &OutfitLimits::default(),
                )?;
                assert_eq!(binding.asset_id, avatar.asset_id);
                assert_eq!(binding.asset_revision, avatar.revision);
                Ok(())
            })
            .unwrap();
        let newer = seed_configuration(&storage, owner, 7);
        assert_eq!(storage.equip(owner, &equip).unwrap(), equipped);
        assert_eq!(storage.outfit_current(owner).unwrap(), newer);
        let same_base = storage
            .equip(owner, &request(&avatar, equipped.revision))
            .unwrap();
        assert_eq!(storage.outfit_current(owner).unwrap().revision, 8);
        assert_eq!(storage.outfit_current(owner).unwrap().base, normalized.base);
        let clear = Equip {
            operation_id: crate::service::library::random_id(),
            clear: true,
            expected_revision: same_base.revision,
            ..Default::default()
        };
        let cleared = storage.equip(owner, &clear).unwrap();
        assert!(cleared.primary.is_none());
        assert!(cleared.figura_compatibility.is_none());
        assert_eq!(
            storage.outfit_current(owner).unwrap(),
            OutfitConfig {
                revision: 9,
                ..Default::default()
            }
        );
        assert_eq!(storage.equip(owner, &clear).unwrap(), cleared);
        assert_eq!(storage.outfit_current(owner).unwrap().revision, 9);
        let vanilla_with_accessories = seed_configuration(&storage, owner, 10);
        assert!(vanilla_with_accessories.base.is_none());
        assert!(!vanilla_with_accessories.instances.is_empty());
        assert_eq!(storage.equip(owner, &clear).unwrap(), cleared);
        assert_eq!(
            storage.outfit_current(owner).unwrap(),
            vanilla_with_accessories
        );
        let clear_again = Equip {
            operation_id: crate::service::library::random_id(),
            clear: true,
            expected_revision: cleared.revision,
            ..Default::default()
        };
        assert_eq!(storage.equip(owner, &clear_again).unwrap(), cleared);
        assert_eq!(
            storage.outfit_current(owner).unwrap(),
            OutfitConfig {
                revision: 11,
                ..Default::default()
            }
        );
        drop(storage);
        let reopened = Storage::open(directory.path()).unwrap();
        assert_eq!(reopened.outfit_current(owner).unwrap().revision, 11);
        assert!(reopened.equipped(owner).unwrap().primary.is_none());
    }

    #[test]
    fn legacy_normalization_failure_rolls_back_equipment_binding_and_operation() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let owner = Uuid::from_u128(1);
        let avatar = native(&storage, owner, "native");
        let before = seed_configuration(&storage, owner, 5);
        let equip = request(&avatar, 0);
        storage.library_read(|database| {
            database.execute_batch("CREATE TEMP TRIGGER fail_legacy_outfit BEFORE INSERT ON outfit_configurations
                BEGIN SELECT RAISE(ABORT, 'synthetic normalization failure'); END;").unwrap();
            Ok(())
        }).unwrap();
        assert!(storage.equip(owner, &equip).is_err());
        assert_eq!(storage.outfit_current(owner).unwrap(), before);
        assert!(storage.equipped(owner).unwrap().primary.is_none());
        storage
            .library_read(|database| {
                let bindings: i64 = database
                    .query_row("SELECT count(*) FROM outfit_local_resources", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(bindings, 0);
                database
                    .execute_batch("DROP TRIGGER fail_legacy_outfit;")
                    .unwrap();
                Ok(())
            })
            .unwrap();
        let mut tight = OutfitLimits::default();
        tight.max_json_bytes = 256;
        assert_eq!(
            storage
                .equip_with_outfit_limits(owner, &equip, &tight)
                .unwrap_err(),
            ErrorCode::InvalidMessage
        );
        assert_eq!(storage.outfit_current(owner).unwrap(), before);
        assert!(storage.equipped(owner).unwrap().primary.is_none());
        let equipped = storage.equip(owner, &equip).unwrap();
        assert_eq!(equipped.primary, Some(avatar));
        assert_eq!(storage.outfit_current(owner).unwrap().revision, 6);
        seed_configuration(&storage, owner, i64::MAX as u64);
        let clear = Equip {
            operation_id: crate::service::library::random_id(),
            clear: true,
            expected_revision: equipped.revision,
            ..Default::default()
        };
        assert_eq!(
            storage.equip(owner, &clear).unwrap_err(),
            ErrorCode::Conflict
        );
        assert_eq!(storage.equipped(owner).unwrap(), equipped);
    }

    #[test]
    fn native_delete_normalizes_only_the_current_base_and_retries_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let owner = Uuid::from_u128(1);
        let base = native(&storage, owner, "base");
        let unused = native(&storage, owner, "unused");
        let equipped = storage.equip(owner, &request(&base, 0)).unwrap();
        let before = seed_configuration(&storage, owner, 5);
        let delete_unused = Delete {
            operation_id: crate::service::library::random_id(),
            asset_id: unused.asset_id,
            expected_revision: unused.revision,
        };
        storage.delete(owner, &delete_unused).unwrap();
        assert_eq!(storage.outfit_current(owner).unwrap(), before);
        assert_eq!(storage.equipped(owner).unwrap().primary, equipped.primary);
        let delete_base = Delete {
            operation_id: crate::service::library::random_id(),
            asset_id: base.asset_id.clone(),
            expected_revision: base.revision,
        };
        storage.library_read(|database| {
            database.execute_batch("CREATE TEMP TRIGGER fail_delete_outfit BEFORE INSERT ON outfit_configurations
                BEGIN SELECT RAISE(ABORT, 'synthetic delete normalization failure'); END;").unwrap();
            Ok(())
        }).unwrap();
        assert!(storage.delete(owner, &delete_base).is_err());
        assert_eq!(
            storage
                .avatar(owner, &base.asset_id, base.revision)
                .unwrap(),
            base
        );
        assert_eq!(storage.equipped(owner).unwrap().primary, equipped.primary);
        assert_eq!(storage.outfit_current(owner).unwrap(), before);
        storage
            .library_read(|database| {
                database
                    .execute_batch("DROP TRIGGER fail_delete_outfit;")
                    .unwrap();
                Ok(())
            })
            .unwrap();
        let deleted = storage.delete(owner, &delete_base).unwrap();
        assert!(storage.equipped(owner).unwrap().primary.is_none());
        assert_eq!(
            storage.outfit_current(owner).unwrap(),
            OutfitConfig {
                revision: 6,
                ..Default::default()
            }
        );
        let newer = seed_configuration(&storage, owner, 7);
        assert_eq!(storage.delete(owner, &delete_base).unwrap(), deleted);
        assert_eq!(storage.outfit_current(owner).unwrap(), newer);
    }

    #[test]
    fn legacy_import_normalizes_replacements_but_preserves_outfits_on_repeated_reads() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let owner = Uuid::from_u128(1);
        let mut avatar = native(&storage, owner, "source");
        avatar.asset_id = "avatar".into();
        avatar.revision = 0;
        seed_configuration(&storage, owner, 5);
        let imported = storage.import_legacy(avatar.clone(), false).unwrap();
        assert_eq!(imported.revision, 1);
        assert_eq!(storage.outfit_current(owner).unwrap().revision, 6);
        let newer = seed_configuration(&storage, owner, 7);
        assert_eq!(
            storage.import_legacy(avatar.clone(), false).unwrap(),
            imported
        );
        assert_eq!(storage.outfit_current(owner).unwrap(), newer);
        assert_eq!(
            storage.import_legacy(avatar.clone(), true).unwrap(),
            imported
        );
        let replaced = storage.outfit_current(owner).unwrap();
        assert_eq!(replaced.revision, 8);
        assert!(replaced.instances.is_empty());
        assert!(replaced.base.is_some());
        let before_equipment = storage.equipped(owner).unwrap();
        let mut tight = OutfitLimits::default();
        tight.max_json_bytes = 256;
        assert_eq!(
            storage
                .import_legacy_with_outfit_limits(avatar.clone(), true, &tight)
                .unwrap_err(),
            ErrorCode::InvalidMessage
        );
        assert_eq!(storage.outfit_current(owner).unwrap(), replaced);
        assert_eq!(storage.equipped(owner).unwrap(), before_equipment);
        storage.library_read(|database| {
            database.execute_batch("CREATE TEMP TRIGGER fail_import_outfit BEFORE INSERT ON outfit_configurations
                BEGIN SELECT RAISE(ABORT, 'synthetic import normalization failure'); END;").unwrap();
            Ok(())
        }).unwrap();
        assert!(storage.import_legacy(avatar.clone(), true).is_err());
        assert_eq!(storage.outfit_current(owner).unwrap(), replaced);
        assert_eq!(storage.equipped(owner).unwrap(), before_equipment);
        storage
            .library_read(|database| {
                database
                    .execute_batch("DROP TRIGGER fail_import_outfit;")
                    .unwrap();
                Ok(())
            })
            .unwrap();
        assert_eq!(storage.import_legacy(avatar, true).unwrap(), imported);
        assert_eq!(storage.outfit_current(owner).unwrap().revision, 9);
    }
}
