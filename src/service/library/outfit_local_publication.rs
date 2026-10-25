//! Explicit promotion of already staged private sources into catalog dependencies.
use super::*;
use crate::protocol::v1::AvatarDescriptor;
use crate::service::{
    outfit::{OutfitConfig, ResourceKind, ResourceRef},
    outfit_limits::OutfitLimits,
};
use crate::storage::outfit_resources::LocalBinding;

pub(super) struct LocalPublication {
    binding: LocalBinding,
    id: Uuid,
    pub descriptor: AvatarDescriptor,
    metadata: Metadata,
    inspected: crate::service::package::PackageIdentity,
}

fn catalog_text(value: &str) -> String {
    let mut plain = String::new();
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character == '§' {
            characters.next();
        } else {
            plain.push(character);
        }
    }
    plain
}

pub(super) fn prepare(
    storage: &Storage,
    actor: Actor,
    input: &super::outfit_uploads::OutfitUploadInput,
    categories: &[String],
    limits: &OutfitLimits,
) -> Result<Vec<LocalPublication>> {
    let mut requested = Vec::new();
    if let Some(base) = &input.configuration.base {
        requested.push((base, ResourceKind::Avatar));
    }
    requested.extend(
        input
            .configuration
            .instances
            .iter()
            .map(|instance| (&instance.resource, ResourceKind::Accessory)),
    );
    let mut prepared: Vec<LocalPublication> = Vec::new();
    for (reference, kind) in requested {
        if !matches!(reference, ResourceRef::Local { .. }) {
            continue;
        }
        if !input.convert_local_dependencies {
            return Err(ErrorCode::InvalidMessage);
        }
        if let Some(prior) = prepared
            .iter()
            .find(|source| source.binding.reference == *reference)
        {
            if prior.binding.resource_kind != kind {
                return Err(ErrorCode::InvalidMessage);
            }
            continue;
        }
        let binding = storage
            .outfit_resolve_local(actor.uuid, reference, kind, limits)?
            .ok_or(ErrorCode::NotFound)?;
        let original = storage.avatar(actor.uuid, &binding.asset_id, binding.asset_revision)?;
        storage.library_ensure_verified_blob(&original)?;
        let inspected =
            crate::service::package::inspect(&storage.blob_path(&original.raw_sha256)?)?;
        let digest = crate::storage::sha256(
            &serde_json::to_vec(&(
                "outfit-local-publication",
                actor.uuid,
                input.operation_id,
                reference,
                kind,
            ))
            .map_err(|_| ErrorCode::Internal)?,
        );
        let id = Uuid::from_slice(&digest[..16]).map_err(|_| ErrorCode::Internal)?;
        let mut descriptor = original;
        descriptor.asset_id = format!("library_{}", id.simple());
        descriptor.revision = 1;
        let name = inspected
            .internal_name
            .as_deref()
            .map(catalog_text)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Local resource".into());
        let metadata = Metadata {
            resource_kind: kind,
            accessory_metadata: binding.accessory_metadata.clone(),
            display_name: name.clone(),
            internal_name: name,
            authors: inspected
                .authors
                .iter()
                .map(|author| catalog_text(author).trim().to_owned())
                .filter(|author| !author.is_empty())
                .collect(),
            model_type: match descriptor.model_type {
                1 => "FIGURA",
                2 => "YSM_PLAIN",
                3 => "YSM_ENCRYPTED",
                _ => return Err(ErrorCode::UnsupportedCapability),
            }
            .into(),
            category_id: input.metadata.category_id.clone(),
            description: String::new(),
            tags: Vec::new(),
            visibility: input.metadata.visibility,
            requires_authorization: input.metadata.requires_authorization,
        };
        metadata.validate(categories)?;
        prepared.push(LocalPublication {
            binding,
            id,
            descriptor,
            metadata,
            inspected,
        });
    }
    Ok(prepared)
}

pub(super) fn promote(
    transaction: &Transaction<'_>,
    actor: Actor,
    name: &str,
    origin: &str,
    configuration: &OutfitConfig,
    prepared: &[LocalPublication],
    limits: &OutfitLimits,
) -> Result<OutfitConfig> {
    let mut configuration = configuration.clone();
    for source in prepared {
        let current = crate::storage::outfit_resources::resolve_local(
            transaction,
            actor.uuid,
            &source.binding.reference,
            source.binding.resource_kind,
            limits,
        )?;
        if current != source.binding {
            return Err(ErrorCode::Conflict);
        }
        let id = source.id.to_string();
        let metadata = &source.metadata;
        let asset = &source.descriptor;
        let now = library::sql_integer(now_seconds())?;
        let visibility = match metadata.visibility {
            Visibility::Public => "PUBLIC",
            Visibility::Private => "PRIVATE",
        };
        transaction
            .execute(
                "INSERT INTO library_models VALUES
            (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '', ?9, ?10, 'ACTIVE', 1, 1, ?11, ?11, 1)",
                params![
                    id,
                    actor.uuid.to_string(),
                    name,
                    metadata.display_name,
                    metadata.internal_name,
                    serde_json::to_string(&metadata.authors).map_err(|_| ErrorCode::Internal)?,
                    metadata.model_type,
                    metadata.category_id,
                    visibility,
                    metadata.requires_authorization,
                    now
                ],
            )
            .map_err(sql_error)?;
        let raw_hash = faster_hex::hex_string(&asset.raw_sha256);
        let metadata_source = if metadata.model_type == "FIGURA" {
            if metadata.resource_kind == ResourceKind::Accessory {
                "cosmetic.json"
            } else {
                "avatar.json"
            }
        } else if metadata.model_type == "YSM_ENCRYPTED" {
            "encrypted_manifest"
        } else {
            "ysm.json"
        };
        transaction
            .execute(
                "INSERT INTO library_revisions VALUES (?1, 1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id,
                    actor.uuid.to_string(),
                    asset.asset_id,
                    raw_hash,
                    library::sql_integer(asset.raw_length)?,
                    asset.distribution_format.to_string(),
                    metadata_source,
                    if metadata.model_type == "YSM_ENCRYPTED" {
                        "CLIENT_EXTRACTED"
                    } else {
                        "SERVER_VERIFIED"
                    },
                    format!("local-{}.zip", source.id.simple())
                ],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO library_resource_kinds VALUES (?1, ?2)",
                params![
                    id,
                    if metadata.resource_kind == ResourceKind::Accessory {
                        "ACCESSORY"
                    } else {
                        "AVATAR"
                    }
                ],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO library_resource_bindings VALUES (?1, ?2, 1, ?3, 1)",
                params![actor.uuid.to_string(), asset.asset_id, id],
            )
            .map_err(sql_error)?;
        if let Some(declaration) = &metadata.accessory_metadata {
            let mut declaration = declaration.clone();
            declaration.validation_source = "CLIENT_EXTRACTED".into();
            transaction
                .execute(
                    "INSERT INTO library_accessory_metadata VALUES (?1, 1, ?2)",
                    params![
                        id,
                        serde_json::to_string(&declaration).map_err(|_| ErrorCode::Internal)?
                    ],
                )
                .map_err(sql_error)?;
        }
        let mut hashes = vec![
            ("RAW_SHA256", asset.raw_sha256.clone()),
            ("NBT_SHA256", source.inspected.nbt_sha256.clone()),
        ];
        if let Some(ciphertext) = &source.inspected.ciphertext_sha256 {
            hashes.push(("YSM_CIPHERTEXT_SHA256", ciphertext.clone()));
        }
        for (kind, hash) in hashes {
            transaction
                .execute(
                    "INSERT INTO library_revision_hashes VALUES (?1, 1, ?2, ?3)",
                    params![id, kind, faster_hex::hex_string(&hash)],
                )
                .map_err(sql_error)?;
        }
        transaction.execute("INSERT INTO library_revision_hashes VALUES (?1, 1, 'LEGACY_FIGURA_BASE64_SHA256', ?2)",
            params![id, asset.legacy_figura_hash]).map_err(sql_error)?;
        super::distribution::record_identity(transaction, asset, &source.inspected)?;
        ensure_uploader_grant(transaction, &id, actor.uuid)?;
        transaction
            .execute(
                "INSERT INTO library_collection (player_uuid, model_id, joined_at, join_source)
            VALUES (?1, ?2, ?3, 'UPLOAD')",
                params![actor.uuid.to_string(), id, now],
            )
            .map_err(sql_error)?;
        advance(transaction, &id)?;
        let replacement = ResourceRef::Cloud {
            origin: origin.into(),
            resource_id: source.id,
            revision: 1,
            content_hash: raw_hash,
            format: metadata.model_type.clone(),
        };
        if configuration.base.as_ref() == Some(&source.binding.reference) {
            configuration.base = Some(replacement.clone());
        }
        for instance in &mut configuration.instances {
            if instance.resource == source.binding.reference {
                instance.resource = replacement.clone();
            }
        }
        if let Some(ordinal) = configuration
            .display_ordinals
            .remove(&source.binding.reference.identity())
        {
            configuration
                .display_ordinals
                .insert(replacement.identity(), ordinal);
        }
    }
    configuration.validate_with_limits(limits)?;
    Ok(configuration)
}
