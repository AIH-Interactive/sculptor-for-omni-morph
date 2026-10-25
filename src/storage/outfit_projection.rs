//! Authorized observer snapshots and revision-scoped source selection.
use super::*;
use crate::service::{
    library::sql_error,
    library_policy::Actor,
    outfit::{AnchorRef, OutfitConfig, ResourceKind, ResourceRef},
    outfit_limits::OutfitLimits,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeSet;

#[derive(Debug, Serialize)]
pub(crate) struct ProjectedResource {
    pub id: String,
    pub reference: ResourceRef,
    pub resource_kind: ResourceKind,
    pub accessory_metadata: Option<crate::service::outfit::AccessoryMetadata>,
    pub distribution_format: i32,
    pub byte_length: u64,
    pub raw_sha256: String,
    pub content_sha256: String,
    pub schema_sha256: String,
    pub relative_path: String,
    // Canonical native asset names are never part of an observer response.
    #[serde(skip)]
    pub descriptor: AvatarDescriptor,
}

#[derive(Debug, Serialize)]
pub(crate) struct Snapshot {
    pub schema_version: u32,
    pub owner: Uuid,
    pub revision: u64,
    pub equipment_revision: u64,
    pub configuration: Option<OutfitConfig>,
    pub resources: Vec<ProjectedResource>,
}

fn available(error: ErrorCode) -> Result<Option<outfit_sources::Source>> {
    match error {
        ErrorCode::Forbidden
        | ErrorCode::NotFound
        | ErrorCode::Conflict
        | ErrorCode::InvalidMessage
        | ErrorCode::UnsupportedCapability => Ok(None),
        error => Err(error),
    }
}

fn source(
    database: &Connection,
    owner: Uuid,
    observer: Uuid,
    origin: &str,
    reference: &ResourceRef,
    kind: ResourceKind,
    limits: &OutfitLimits,
    require_verified: bool,
) -> Result<Option<outfit_sources::Source>> {
    let actor = Actor {
        uuid: owner,
        admin_context: false,
        granted: false,
    };
    let resolved = match outfit_sources::resolve(database, actor, origin, reference, kind, limits) {
        Ok(source) => source,
        Err(error) => return available(error),
    };
    if !resolved.can_render(database, reference, owner, observer, true)? {
        return Ok(None);
    }
    if require_verified {
        let verified: bool = database
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM library_verified_blobs WHERE raw_sha256 = ?1)",
                [faster_hex::hex_string(&resolved.descriptor.raw_sha256)],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if !verified {
            return Ok(None);
        }
    }
    Ok(Some(resolved))
}

fn resource(
    owner: Uuid,
    revision: u64,
    reference: &ResourceRef,
    kind: ResourceKind,
    source: outfit_sources::Source,
) -> Result<ProjectedResource> {
    let identity =
        serde_json::to_vec(&(owner, reference, kind)).map_err(|_| ErrorCode::Internal)?;
    let id = faster_hex::hex_string(&sha256(&identity));
    Ok(ProjectedResource {
        relative_path: format!(
            "/api/library/v1/outfits/players/{owner}/resources/{id}?revision={revision}"
        ),
        id,
        reference: reference.clone(),
        resource_kind: kind,
        accessory_metadata: source.metadata,
        distribution_format: source.descriptor.distribution_format,
        byte_length: source.descriptor.raw_length,
        raw_sha256: faster_hex::hex_string(&source.descriptor.raw_sha256),
        content_sha256: faster_hex::hex_string(&source.descriptor.content_sha256),
        schema_sha256: faster_hex::hex_string(&source.descriptor.schema_sha256),
        descriptor: source.descriptor,
    })
}

/// Remove dependents transitively; manual bindings never silently choose another parent.
fn prune(configuration: &mut OutfitConfig) {
    loop {
        let before = (
            configuration.instances.len(),
            configuration.virtual_anchors.len(),
        );
        let ids: BTreeSet<_> = configuration
            .instances
            .iter()
            .map(|value| value.instance_id)
            .collect();
        configuration
            .virtual_anchors
            .retain(|anchor| anchor.parent.instance_id.is_none_or(|id| ids.contains(&id)));
        let virtuals: BTreeSet<_> = configuration
            .virtual_anchors
            .iter()
            .map(|value| value.id)
            .collect();
        configuration
            .instances
            .retain(|instance| match &instance.anchor {
                AnchorRef::AccessoryBone { bone } => {
                    bone.instance_id.is_some_and(|id| ids.contains(&id))
                }
                AnchorRef::Virtual { anchor_id } => virtuals.contains(anchor_id),
                _ => true,
            });
        if before
            == (
                configuration.instances.len(),
                configuration.virtual_anchors.len(),
            )
        {
            break;
        }
    }
    let ids: BTreeSet<_> = configuration
        .instances
        .iter()
        .map(|value| value.instance_id)
        .collect();
    for instance in &mut configuration.instances {
        if let AnchorRef::AutoTrail {
            preferred_instance_id,
        } = &mut instance.anchor
        {
            if preferred_instance_id.is_some_and(|id| !ids.contains(&id)) {
                *preferred_instance_id = None;
            }
        }
    }
    let identities: BTreeSet<_> = configuration
        .instances
        .iter()
        .map(|value| value.resource.identity())
        .collect();
    configuration
        .display_ordinals
        .retain(|identity, _| identities.contains(identity));
}

impl Storage {
    /// This URL authorizes rendering one dependency of one current configuration revision.
    pub(crate) fn outfit_resource(
        &self,
        owner: Uuid,
        observer: Uuid,
        origin: &str,
        revision: u64,
        id: &str,
        limits: &OutfitLimits,
    ) -> Result<AvatarDescriptor> {
        library::validate_hash("RAW_SHA256", id)?;
        library::sql_integer(revision)?;
        let snapshot = self.outfit_project(owner, observer, origin, limits)?;
        if snapshot.revision != revision {
            return Err(ErrorCode::Conflict);
        }
        snapshot
            .resources
            .into_iter()
            .find(|resource| resource.id == id)
            .map(|resource| resource.descriptor)
            .ok_or(ErrorCode::NotFound)
    }

    /// One SQLite snapshot covers configuration, native base, policies and both players' grants.
    /// This metadata-only method does not authorize any native resource download route.
    pub(crate) fn outfit_project(
        &self,
        owner: Uuid,
        observer: Uuid,
        origin: &str,
        limits: &OutfitLimits,
    ) -> Result<Snapshot> {
        let candidate = self.library_transaction(|database| {
            project(database, owner, observer, origin, limits, false)
        })?;
        // Legacy imported bodies may not yet have a server-computed package identity.
        // Do disk inspection outside the SQLite lock, then re-read current policy/config.
        for resource in candidate.resources {
            self.library_ensure_verified_blob(&resource.descriptor)?;
        }
        self.library_transaction(|database| {
            project(database, owner, observer, origin, limits, true)
        })
    }
}

/// Additional guard for the old native body boundary, including historical asset URLs.
/// This does not change unmarked legacy assets or the owner's existing private reads.
pub(crate) fn native_access(
    database: &Connection,
    owner: Uuid,
    observer: Uuid,
    asset: &str,
    revision: u64,
    limits: &OutfitLimits,
) -> Result<()> {
    if owner == observer {
        return Ok(());
    }
    let private: bool = database
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM outfit_private_sources
         WHERE owner_uuid = ?1 AND asset_id = ?2 AND asset_revision = ?3)",
            params![owner.to_string(), asset, library::sql_integer(revision)?],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if !private {
        return Ok(());
    }
    let json: Option<String> = database
        .query_row(
            "SELECT configuration_json FROM outfit_configurations WHERE owner_uuid = ?1",
            [owner.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let Some(json) = json else {
        return Err(ErrorCode::NotFound);
    };
    let configuration: OutfitConfig =
        serde_json::from_str(&json).map_err(|_| ErrorCode::Internal)?;
    // The stored source label selects no external host; all resolution stays in this database.
    let origin = configuration
        .base
        .iter()
        .chain(
            configuration
                .instances
                .iter()
                .map(|instance| &instance.resource),
        )
        .find_map(|reference| match reference {
            ResourceRef::Cloud { origin, .. } => Some(origin.as_str()),
            ResourceRef::Local { .. } => None,
        })
        .unwrap_or("");
    let projected = project(database, owner, observer, origin, limits, true)?;
    if projected.resources.iter().any(|resource| {
        resource.descriptor.owner == owner.to_string()
            && resource.descriptor.asset_id == asset
            && resource.descriptor.revision == revision
    }) {
        Ok(())
    } else {
        Err(ErrorCode::NotFound)
    }
}

pub(super) fn project(
    database: &Connection,
    owner: Uuid,
    observer: Uuid,
    origin: &str,
    limits: &OutfitLimits,
    require_verified: bool,
) -> Result<Snapshot> {
    let encoded: Option<String> = database
        .query_row(
            "SELECT configuration_json FROM outfit_configurations WHERE owner_uuid = ?1",
            [owner.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let mut configuration: OutfitConfig = encoded
        .map(|json| serde_json::from_str(&json).map_err(|_| ErrorCode::Internal))
        .transpose()?
        .unwrap_or_default();
    let equipment_revision: Option<i64> = database
        .query_row(
            "SELECT revision FROM owners WHERE uuid = ?1",
            [owner.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    let mut snapshot = Snapshot {
        schema_version: 1,
        owner,
        revision: configuration.revision,
        equipment_revision: u64::try_from(equipment_revision.unwrap_or(0))
            .map_err(|_| ErrorCode::Internal)?,
        configuration: None,
        resources: Vec::new(),
    };
    if (owner != observer && !configuration.show_equipment)
        || configuration.validate_with_limits(limits).is_err()
    {
        return Ok(snapshot);
    }
    let primary: Option<(String, i64, Vec<u8>, bool)> = database
        .query_row(
            "SELECT e.asset_id, e.revision, a.raw_sha256,
                 EXISTS(SELECT 1 FROM library_wardrobe_settings w WHERE w.owner_uuid = e.owner_uuid
                   AND w.asset_id = e.asset_id AND w.asset_revision = e.revision AND w.show_equipment = 0)
                 FROM equipment e JOIN avatars a ON a.owner_uuid = e.owner_uuid
                   AND a.asset_id = e.asset_id AND a.revision = e.revision AND a.tombstone = 0
                 WHERE e.owner_uuid = ?1 AND e.slot = 'primary'",
            [owner.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(sql_error)?;
    if let Some(reference) = &configuration.base {
        let Some(resolved) = source(
            database,
            owner,
            observer,
            origin,
            reference,
            ResourceKind::Avatar,
            limits,
            require_verified,
        )?
        else {
            return Ok(snapshot);
        };
        let (asset, revision) = match reference {
            ResourceRef::Local { .. } => (
                resolved.descriptor.asset_id.clone(),
                resolved.descriptor.revision,
            ),
            ResourceRef::Cloud {
                resource_id,
                revision,
                ..
            } => (format!("lib_{}_{}", resource_id.simple(), revision), 1),
        };
        if let ResourceRef::Cloud {
            resource_id,
            revision: source_revision,
            ..
        } = reference
        {
            let bound: bool = database
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM library_resource_bindings WHERE resource_owner = ?1
                         AND asset_id = ?2 AND revision = 1 AND source_model_id = ?3 AND source_revision = ?4)",
                    params![
                        owner.to_string(),
                        asset,
                        resource_id.to_string(),
                        library::sql_integer(*source_revision)?
                    ],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            if !bound {
                return Ok(snapshot);
            }
            if owner != observer
                && resolved.policy.is_some_and(|policy| {
                    policy.visibility == crate::service::library_policy::Visibility::Private
                })
            {
                let visible: bool = database
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM library_equipment_settings WHERE resource_owner = ?1
                             AND asset_id = ?2 AND revision = 1 AND show_private_equipment = 1)",
                        params![owner.to_string(), asset],
                        |row| row.get(0),
                    )
                    .map_err(sql_error)?;
                if !visible {
                    return Ok(snapshot);
                }
            }
        }
        if primary.as_ref().is_none_or(|(id, rev, hash, hidden)| {
            id != &asset
                || *rev != revision as i64
                || hash != &resolved.descriptor.raw_sha256
                || (owner != observer && *hidden)
        }) {
            return Ok(snapshot);
        }
        snapshot.resources.push(resource(
            owner,
            configuration.revision,
            reference,
            ResourceKind::Avatar,
            resolved,
        )?);
    } else if primary.is_some() {
        return Ok(snapshot);
    }
    let mut declarations = BTreeMap::new();
    let mut retained = Vec::new();
    for instance in &configuration.instances {
        if !instance.enabled {
            continue;
        }
        let Some(resolved) = source(
            database,
            owner,
            observer,
            origin,
            &instance.resource,
            ResourceKind::Accessory,
            limits,
            require_verified,
        )?
        else {
            tracing::debug!(%owner, %observer, instance = %instance.instance_id,
                code = "NOT_DISTRIBUTABLE", "Outfit instance omitted by source policy");
            continue;
        };
        tracing::debug!(%owner, %observer, instance = %instance.instance_id,
            asset_id = %resolved.descriptor.asset_id, revision = resolved.descriptor.revision,
            visibility = ?resolved.policy.map(|policy| policy.visibility),
            moderation = ?resolved.policy.map(|policy| policy.moderation),
            "Outfit instance asset projected");
        declarations.insert(
            instance.instance_id,
            resolved.metadata.clone().ok_or(ErrorCode::Internal)?,
        );
        snapshot.resources.push(resource(
            owner,
            configuration.revision,
            &instance.resource,
            ResourceKind::Accessory,
            resolved,
        )?);
        retained.push(instance.clone());
    }
    configuration.instances = retained;
    prune(&mut configuration);
    if crate::service::outfit_capabilities::validate(&configuration, &declarations, limits).is_err()
    {
        snapshot.resources.clear();
        return Ok(snapshot);
    }
    snapshot.resources.retain(|resource| {
        resource.resource_kind == ResourceKind::Avatar
            || configuration
                .instances
                .iter()
                .any(|instance| instance.resource == resource.reference)
    });
    let mut unique = BTreeSet::new();
    snapshot
        .resources
        .retain(|resource| unique.insert(resource.id.clone()));
    snapshot.configuration = Some(configuration);
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::outfit::{
        AccessoryInstance, BoneRef, BoneSegment, Transform, VirtualAnchor,
    };

    #[test]
    fn outfit_projection_prunes_manual_dependents_without_retargeting_or_renumbering() {
        let reference = ResourceRef::Local {
            local_id: Uuid::from_u128(1),
            content_hash: "a".repeat(64),
            format: "FIGURA".into(),
        };
        let bone = |id| BoneRef {
            instance_id: Some(Uuid::from_u128(id)),
            path: vec![BoneSegment {
                name: "Root".into(),
                sibling_ordinal: 0,
            }],
            skeleton_hash: "b".repeat(64),
        };
        let instance = |id, anchor| AccessoryInstance {
            instance_id: Uuid::from_u128(id),
            resource: reference.clone(),
            display_ordinal: id as u32,
            enabled: true,
            anchor,
            transform: Transform::default(),
            bone_overrides: Vec::new(),
            controls: Default::default(),
            lua_state: Default::default(),
        };
        let mut configuration = OutfitConfig::default();
        configuration
            .display_ordinals
            .insert(reference.identity(), 9);
        configuration
            .display_ordinals
            .insert("local:hidden-source".into(), 4);
        // Parent 1 has already been filtered out. Order deliberately requires another pass.
        configuration.instances = vec![
            instance(
                3,
                AnchorRef::Virtual {
                    anchor_id: Uuid::from_u128(100),
                },
            ),
            instance(2, AnchorRef::AccessoryBone { bone: bone(1) }),
            instance(
                4,
                AnchorRef::AutoTrail {
                    preferred_instance_id: Some(Uuid::from_u128(2)),
                },
            ),
            instance(
                5,
                AnchorRef::Standard {
                    name: "head_anchor".into(),
                },
            ),
        ];
        configuration.virtual_anchors.push(VirtualAnchor {
            id: Uuid::from_u128(100),
            name: "Parent 2 export".into(),
            parent: bone(2),
            transform: Transform::default(),
        });
        prune(&mut configuration);
        assert!(configuration.virtual_anchors.is_empty());
        assert_eq!(
            configuration
                .instances
                .iter()
                .map(|instance| instance.instance_id)
                .collect::<Vec<_>>(),
            vec![Uuid::from_u128(4), Uuid::from_u128(5)]
        );
        assert_eq!(
            configuration.instances[0].anchor,
            AnchorRef::AutoTrail {
                preferred_instance_id: None
            }
        );
        assert_eq!(configuration.instances[0].display_ordinal, 4);
        assert_eq!(configuration.instances[1].display_ordinal, 5);
        assert_eq!(configuration.display_ordinals.len(), 1);
        assert_eq!(configuration.display_ordinals[&reference.identity()], 9);
        configuration.validate().unwrap();
    }
}
