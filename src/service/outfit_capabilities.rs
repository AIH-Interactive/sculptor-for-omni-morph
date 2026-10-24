//! Apply-time author declarations. Actual skeletons remain a client runtime check.
use super::{outfit::*, outfit_limits::OutfitLimits};
use crate::protocol::v1::ErrorCode;
use std::collections::BTreeMap;
use uuid::Uuid;

type Result<T> = std::result::Result<T, ErrorCode>;

fn relative(bone: &BoneRef) -> BoneRef {
    BoneRef {
        instance_id: None,
        path: bone.path.clone(),
        skeleton_hash: bone.skeleton_hash.clone(),
    }
}

fn exported(bone: &BoneRef, declarations: &BTreeMap<Uuid, AccessoryMetadata>) -> Result<()> {
    let Some(instance) = bone.instance_id else {
        // Base skeleton identity is checked after native loading, including vanilla.
        return Ok(());
    };
    let declaration = declarations
        .get(&instance)
        .ok_or(ErrorCode::InvalidMessage)?;
    if !declaration.has_bones {
        return Err(ErrorCode::Forbidden);
    }
    if declaration.skeleton_hash != bone.skeleton_hash {
        return Err(ErrorCode::Conflict);
    }
    if !declaration.exported_bones.contains(&relative(bone)) {
        return Err(ErrorCode::Forbidden);
    }
    Ok(())
}

/// Call after graph validation and fresh permission checks for every fixed resource.
pub(crate) fn validate(
    configuration: &OutfitConfig,
    declarations: &BTreeMap<Uuid, AccessoryMetadata>,
    limits: &OutfitLimits,
) -> Result<()> {
    limits.validate()?;
    let base = match configuration.base.as_ref() {
        None => "VANILLA",
        Some(ResourceRef::Local { format, .. } | ResourceRef::Cloud { format, .. }) => {
            if format == "FIGURA" { "FIGURA" } else { "YSM" }
        }
    };
    let mut trails = 0;
    for instance in &configuration.instances {
        let declaration = declarations
            .get(&instance.instance_id)
            .ok_or(ErrorCode::InvalidMessage)?;
        declaration.validate_with_limits(limits)?;
        if !declaration
            .supported_bases
            .iter()
            .any(|supported| supported == base)
        {
            return Err(ErrorCode::UnsupportedCapability);
        }
        if instance.enabled && declaration.category == "Trail" {
            trails += 1;
            if trails > limits.max_trails {
                return Err(ErrorCode::InvalidMessage);
            }
        }
        if !instance.bone_overrides.is_empty() {
            if !declaration.has_bones || !declaration.edit_bones {
                return Err(ErrorCode::Forbidden);
            }
            for entry in &instance.bone_overrides {
                if entry.bone.skeleton_hash != declaration.skeleton_hash {
                    return Err(ErrorCode::Conflict);
                }
                if !declaration.editable_bones.contains(&relative(&entry.bone)) {
                    return Err(ErrorCode::Forbidden);
                }
            }
        }
        match &instance.anchor {
            AnchorRef::AccessoryBone { bone } => exported(bone, declarations)?,
            // AutoTrail may fall back to the base, hip, body or root. Its preferred
            // instance is not a request to access an undeclared internal bone.
            AnchorRef::Standard { .. }
            | AnchorRef::BaseBone { .. }
            | AnchorRef::Virtual { .. }
            | AnchorRef::AutoTrail { .. }
            | AnchorRef::Unresolved { .. } => {}
        }
    }
    for anchor in &configuration.virtual_anchors {
        exported(&anchor.parent, declarations)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bone(instance_id: Option<Uuid>, name: &str) -> BoneRef {
        BoneRef {
            instance_id,
            path: vec![
                BoneSegment {
                    name: "Root".into(),
                    sibling_ordinal: 0,
                },
                BoneSegment {
                    name: name.into(),
                    sibling_ordinal: 1,
                },
            ],
            skeleton_hash: "a".repeat(64),
        }
    }

    fn fixture() -> (OutfitConfig, BTreeMap<Uuid, AccessoryMetadata>) {
        let resource = ResourceRef::Local {
            local_id: Uuid::from_u128(10),
            content_hash: "b".repeat(64),
            format: "FIGURA".into(),
        };
        let declaration = AccessoryMetadata {
            schema_version: 1,
            category: "Tail".into(),
            cosmetic_anchor: "wing_anchor".into(),
            default_transform: Transform::default(),
            supported_bases: vec!["VANILLA".into(), "FIGURA".into(), "YSM".into()],
            has_bones: true,
            skeleton_revision: "one".into(),
            skeleton_hash: "a".repeat(64),
            display_bones: false,
            edit_bones: true,
            exported_bones: vec![bone(None, "Tip")],
            editable_bones: vec![bone(None, "Inner")],
            validation_source: "CLIENT_EXTRACTED".into(),
        };
        let mut configuration = OutfitConfig::default();
        configuration
            .display_ordinals
            .insert(resource.identity(), 2);
        let mut declarations = BTreeMap::new();
        for ordinal in 1..=2 {
            let id = Uuid::from_u128(ordinal);
            configuration.instances.push(AccessoryInstance {
                instance_id: id,
                resource: resource.clone(),
                display_ordinal: ordinal as u32,
                enabled: true,
                anchor: AnchorRef::Standard {
                    name: "wing_anchor".into(),
                },
                transform: Transform::default(),
                bone_overrides: Vec::new(),
                controls: BTreeMap::new(),
                lua_state: BTreeMap::new(),
            });
            declarations.insert(id, declaration.clone());
        }
        configuration.instances[1].anchor = AnchorRef::AccessoryBone {
            bone: bone(Some(Uuid::from_u128(1)), "Tip"),
        };
        configuration.instances[1]
            .bone_overrides
            .push(BoneOverride {
                bone: bone(Some(Uuid::from_u128(2)), "Inner"),
                transform: Transform::default(),
            });
        configuration.validate().unwrap();
        (configuration, declarations)
    }

    #[test]
    fn declared_exports_and_edits_are_exactly_instance_path_and_hash_scoped() {
        let (configuration, declarations) = fixture();
        let limits = OutfitLimits::default();
        assert!(validate(&configuration, &declarations, &limits).is_ok());
        let mut private = configuration.clone();
        private.instances[1].anchor = AnchorRef::AccessoryBone {
            bone: bone(Some(Uuid::from_u128(1)), "Private"),
        };
        assert_eq!(
            validate(&private, &declarations, &limits),
            Err(ErrorCode::Forbidden)
        );
        let mut stale = configuration.clone();
        let mut target = bone(Some(Uuid::from_u128(1)), "Tip");
        target.skeleton_hash = "c".repeat(64);
        stale.instances[1].anchor = AnchorRef::AccessoryBone { bone: target };
        assert_eq!(
            validate(&stale, &declarations, &limits),
            Err(ErrorCode::Conflict)
        );
        let mut outside = configuration.clone();
        outside.instances[1].bone_overrides[0].bone = bone(Some(Uuid::from_u128(2)), "Tip");
        assert_eq!(
            validate(&outside, &declarations, &limits),
            Err(ErrorCode::Forbidden)
        );
        let mut no_edit = declarations.clone();
        let declaration = no_edit.get_mut(&Uuid::from_u128(2)).unwrap();
        declaration.edit_bones = false;
        declaration.editable_bones.clear();
        assert_eq!(
            validate(&configuration, &no_edit, &limits),
            Err(ErrorCode::Forbidden)
        );
        let mut sibling = configuration.clone();
        sibling.instances[1].bone_overrides[0].bone.path[1].sibling_ordinal = 0;
        assert_eq!(
            validate(&sibling, &declarations, &limits),
            Err(ErrorCode::Forbidden)
        );
    }

    #[test]
    fn virtual_parent_exports_and_disabled_instances_keep_the_same_author_boundaries() {
        let (mut configuration, declarations) = fixture();
        let limits = OutfitLimits::default();
        let anchor_id = Uuid::from_u128(3);
        configuration.virtual_anchors.push(VirtualAnchor {
            id: anchor_id,
            name: "Shared".into(),
            parent: bone(Some(Uuid::from_u128(1)), "Tip"),
            transform: Transform::default(),
        });
        configuration.instances[1].anchor = AnchorRef::Virtual { anchor_id };
        configuration.instances[0].enabled = false;
        configuration.validate().unwrap();
        assert!(validate(&configuration, &declarations, &limits).is_ok());
        configuration.virtual_anchors[0].parent = bone(Some(Uuid::from_u128(1)), "Private");
        assert_eq!(
            validate(&configuration, &declarations, &limits),
            Err(ErrorCode::Forbidden)
        );
        configuration.virtual_anchors.clear();
        configuration.instances[1].anchor = AnchorRef::Standard {
            name: "wing_anchor".into(),
        };
        configuration.instances[1].enabled = false;
        configuration.instances[1].bone_overrides[0]
            .bone
            .skeleton_hash = "c".repeat(64);
        assert_eq!(
            validate(&configuration, &declarations, &limits),
            Err(ErrorCode::Conflict)
        );
    }

    #[test]
    fn supported_base_families_and_current_declaration_budgets_are_enforced() {
        let (mut configuration, mut declarations) = fixture();
        let mut limits = OutfitLimits::default();
        for declaration in declarations.values_mut() {
            declaration.supported_bases = vec!["YSM".into()];
        }
        assert_eq!(
            validate(&configuration, &declarations, &limits),
            Err(ErrorCode::UnsupportedCapability)
        );
        configuration.base = Some(ResourceRef::Local {
            local_id: Uuid::from_u128(99),
            content_hash: "c".repeat(64),
            format: "YSM_ENCRYPTED".into(),
        });
        assert!(validate(&configuration, &declarations, &limits).is_ok());
        if let Some(ResourceRef::Local { format, .. }) = &mut configuration.base {
            *format = "YSM_PLAIN".into();
        }
        assert!(validate(&configuration, &declarations, &limits).is_ok());
        if let Some(ResourceRef::Local { format, .. }) = &mut configuration.base {
            *format = "FIGURA".into();
        }
        assert_eq!(
            validate(&configuration, &declarations, &limits),
            Err(ErrorCode::UnsupportedCapability)
        );
        for declaration in declarations.values_mut() {
            declaration.supported_bases = vec!["FIGURA".into()];
        }
        assert!(validate(&configuration, &declarations, &limits).is_ok());
        limits.max_exported_bones = 0;
        assert_eq!(
            validate(&configuration, &declarations, &limits),
            Err(ErrorCode::InvalidMessage)
        );
        limits.max_exported_bones = 128;
        limits.max_trails = 1;
        for declaration in declarations.values_mut() {
            declaration.category = "Trail".into();
        }
        assert_eq!(
            validate(&configuration, &declarations, &limits),
            Err(ErrorCode::InvalidMessage)
        );
        configuration.instances[0].enabled = false;
        assert!(validate(&configuration, &declarations, &limits).is_ok());
    }
}
