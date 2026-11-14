//! Versioned JSON outfit contract. This module never executes model scripts.
use super::outfit_limits::OutfitLimits;
use crate::protocol::v1::ErrorCode;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

type Result<T> = std::result::Result<T, ErrorCode>;

pub const MAX_INSTANCES: usize = 32;
pub const MAX_ANCHORS: usize = 128;
pub const MAX_DEPTH: usize = 64;
pub const MAX_JSON_BYTES: usize = 65536;

/// Operational rollback switch. Existing owner sources and primary models remain available.
pub fn asset_distribution_enabled() -> bool {
    std::env::var("OUTFIT_ASSET_DISTRIBUTION")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResourceKind {
    #[default]
    Avatar,
    Accessory,
    Outfit,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AccessoryMetadata {
    pub schema_version: u32,
    pub category: String,
    pub cosmetic_anchor: String,
    pub default_transform: Transform,
    pub supported_bases: Vec<String>,
    pub has_bones: bool,
    pub skeleton_revision: String,
    pub skeleton_hash: String,
    pub display_bones: bool,
    pub edit_bones: bool,
    pub exported_bones: Vec<BoneRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editable_bones: Vec<BoneRef>,
    pub validation_source: String,
}

impl AccessoryMetadata {
    pub fn validate(&self) -> Result<()> {
        self.validate_with_limits(&OutfitLimits::default())
    }

    pub fn validate_with_limits(&self, limits: &OutfitLimits) -> Result<()> {
        limits.validate()?;
        if self.schema_version != 1
            || self.supported_bases.len() > 3
            || self.exported_bones.len() > limits.max_exported_bones
            || self.editable_bones.len() > limits.max_resource_nodes
            || (!self.edit_bones && !self.editable_bones.is_empty())
            || (!self.has_bones
                && (self.display_bones || self.edit_bones || !self.exported_bones.is_empty()))
            || !matches!(
                self.validation_source.as_str(),
                "SERVER_VERIFIED" | "CLIENT_EXTRACTED"
            )
        {
            return Err(ErrorCode::InvalidMessage);
        }
        super::library::text(&self.category, 64, false)?;
        super::library::text(&self.cosmetic_anchor, 128, true)?;
        super::library::text(&self.skeleton_revision, 128, true)?;
        self.default_transform.validate_with_limits(limits)?;
        if self.has_bones {
            crate::storage::library::validate_hash("RAW_SHA256", &self.skeleton_hash)?;
        } else if !self.skeleton_hash.is_empty() {
            return Err(ErrorCode::InvalidMessage);
        }
        let mut bases = BTreeSet::new();
        for base in &self.supported_bases {
            if !matches!(base.as_str(), "VANILLA" | "FIGURA" | "YSM") || !bases.insert(base) {
                return Err(ErrorCode::InvalidMessage);
            }
        }
        for range in [&self.exported_bones, &self.editable_bones] {
            let mut seen = std::collections::HashSet::new();
            for bone in range {
                bone.validate_with_limits(limits)?;
                // Resource declarations never embed a player's instance UUID.
                if bone.instance_id.is_some()
                    || bone.skeleton_hash != self.skeleton_hash
                    || !seen.insert(bone)
                {
                    return Err(ErrorCode::InvalidMessage);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "source",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
pub enum ResourceRef {
    Local {
        local_id: Uuid,
        content_hash: String,
        format: String,
    },
    Cloud {
        origin: String,
        resource_id: Uuid,
        revision: u64,
        content_hash: String,
        format: String,
    },
}

impl ResourceRef {
    pub fn validate(&self) -> Result<()> {
        let (hash, format) = match self {
            Self::Local {
                content_hash,
                format,
                ..
            } => (content_hash, format),
            Self::Cloud {
                origin,
                revision,
                content_hash,
                format,
                ..
            } => {
                let url = reqwest::Url::parse(origin).map_err(|_| ErrorCode::InvalidMessage)?;
                if *revision == 0
                    || *revision > i64::MAX as u64
                    || !matches!(url.scheme(), "https" | "http")
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.path() != "/"
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || url.origin().ascii_serialization() != *origin
                {
                    return Err(ErrorCode::InvalidMessage);
                }
                (content_hash, format)
            }
        };
        crate::storage::library::validate_hash("RAW_SHA256", hash)?;
        if !matches!(format.as_str(), "FIGURA" | "YSM_PLAIN" | "YSM_ENCRYPTED") {
            return Err(ErrorCode::InvalidMessage);
        }
        Ok(())
    }

    pub fn identity(&self) -> String {
        match self {
            Self::Local { local_id, .. } => format!("local:{local_id}"),
            Self::Cloud {
                origin,
                resource_id,
                ..
            } => format!("{origin}:{resource_id}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Transform {
    pub position: [f64; 3],
    pub rotation: [f64; 3],
    pub scale: [f64; 3],
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            position: [0.0; 3],
            rotation: [0.0; 3],
            scale: [1.0; 3],
        }
    }
}

impl Transform {
    pub fn validate_with_limits(&self, limits: &OutfitLimits) -> Result<()> {
        if self
            .position
            .iter()
            .any(|v| !v.is_finite() || v.abs() > limits.position_limit)
            || self
                .rotation
                .iter()
                .any(|v| !v.is_finite() || v.abs() > limits.rotation_limit)
            || self
                .scale
                .iter()
                .any(|v| !v.is_finite() || !(limits.min_scale..=limits.max_scale).contains(v))
        {
            return Err(ErrorCode::InvalidMessage);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct BoneSegment {
    pub name: String,
    pub sibling_ordinal: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct BoneRef {
    /// None denotes the base skeleton; a UUID denotes exactly one accessory instance.
    pub instance_id: Option<Uuid>,
    pub path: Vec<BoneSegment>,
    pub skeleton_hash: String,
}

impl BoneRef {
    pub fn validate_with_limits(&self, limits: &OutfitLimits) -> Result<()> {
        if self.path.is_empty() || self.path.len() > limits.max_depth {
            return Err(ErrorCode::InvalidMessage);
        }
        crate::storage::library::validate_hash("RAW_SHA256", &self.skeleton_hash)?;
        for segment in &self.path {
            super::library::text(&segment.name, 128, false)?;
            if segment.sibling_ordinal > 8191 {
                return Err(ErrorCode::InvalidMessage);
            }
        }
        Ok(())
    }
}

pub const STANDARD_ANCHORS: [&str; 11] = [
    "head_anchor",
    "face_anchor",
    "hair_anchor",
    "back_anchor",
    "wing_anchor",
    "hip",
    "tailtip",
    "left_leg_anchor",
    "right_leg_anchor",
    "left_foot_anchor",
    "right_foot_anchor",
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum AnchorRef {
    Standard {
        name: String,
    },
    BaseBone {
        bone: BoneRef,
    },
    Virtual {
        anchor_id: Uuid,
    },
    AccessoryBone {
        bone: BoneRef,
    },
    AutoTrail {
        preferred_instance_id: Option<Uuid>,
    },
    /// An unresolved author symbol remains visible for rebinding, never guesses a bone.
    Unresolved {
        symbol: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VirtualAnchor {
    pub id: Uuid,
    pub name: String,
    pub parent: BoneRef,
    pub transform: Transform,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BoneOverride {
    pub bone: BoneRef,
    pub transform: Transform,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AccessoryInstance {
    pub instance_id: Uuid,
    pub resource: ResourceRef,
    pub display_ordinal: u32,
    pub enabled: bool,
    pub anchor: AnchorRef,
    pub transform: Transform,
    pub bone_overrides: Vec<BoneOverride>,
    pub controls: BTreeMap<String, serde_json::Value>,
    pub lua_state: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OutfitConfig {
    pub schema_version: u32,
    pub revision: u64,
    pub base: Option<ResourceRef>,
    pub instances: Vec<AccessoryInstance>,
    pub virtual_anchors: Vec<VirtualAnchor>,
    /// Allocation watermarks survive deleting the last copy and saving/reloading.
    pub display_ordinals: BTreeMap<String, u32>,
    pub show_equipment: bool,
}

impl Default for OutfitConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            revision: 0,
            base: None,
            instances: Vec::new(),
            virtual_anchors: Vec::new(),
            display_ordinals: BTreeMap::new(),
            show_equipment: false,
        }
    }
}

impl OutfitConfig {
    pub fn validate(&self) -> Result<()> {
        self.validate_with_limits(&OutfitLimits::default())
    }

    pub fn validate_with_limits(&self, limits: &OutfitLimits) -> Result<()> {
        limits.validate()?;
        if self.schema_version != 1
            || self.revision > i64::MAX as u64
            || self.instances.len() > limits.max_instances
            || self.virtual_anchors.len() > limits.max_virtual_anchors
            || self.display_ordinals.len() > limits.max_display_ordinals
            || serde_json::to_vec(self)
                .map_err(|_| ErrorCode::InvalidMessage)?
                .len()
                > limits.max_json_bytes
        {
            return Err(ErrorCode::InvalidMessage);
        }
        if let Some(base) = &self.base {
            base.validate()?;
        }
        for (identity, ordinal) in &self.display_ordinals {
            super::library::text(identity, 512, false)?;
            if *ordinal == 0 || *ordinal > i32::MAX as u32 {
                return Err(ErrorCode::InvalidMessage);
            }
        }
        let mut ids = BTreeSet::new();
        for instance in &self.instances {
            if !ids.insert(instance.instance_id) {
                return Err(ErrorCode::InvalidMessage);
            }
        }
        let mut virtuals = BTreeMap::new();
        for anchor in &self.virtual_anchors {
            if ids.contains(&anchor.id) || virtuals.insert(anchor.id, anchor).is_some() {
                return Err(ErrorCode::InvalidMessage);
            }
            super::library::text(&anchor.name, 128, false)?;
            anchor.parent.validate_with_limits(limits)?;
            anchor.transform.validate_with_limits(limits)?;
            if anchor
                .parent
                .instance_id
                .is_some_and(|id| !ids.contains(&id))
            {
                return Err(ErrorCode::InvalidMessage);
            }
        }
        let mut edges = BTreeMap::new();
        let mut ordinals = BTreeSet::new();
        let mut overrides = 0;
        for instance in &self.instances {
            instance.resource.validate()?;
            instance.transform.validate_with_limits(limits)?;
            let identity = instance.resource.identity();
            if instance.display_ordinal == 0
                || instance.display_ordinal > i32::MAX as u32
                || !ordinals.insert((identity.clone(), instance.display_ordinal))
                || self
                    .display_ordinals
                    .get(&identity)
                    .is_none_or(|max| *max < instance.display_ordinal)
            {
                return Err(ErrorCode::InvalidMessage);
            }
            for (values, maximum) in [
                (&instance.controls, limits.max_controls),
                (&instance.lua_state, limits.max_lua_state_entries),
            ] {
                if values.len() > maximum {
                    return Err(ErrorCode::InvalidMessage);
                }
                for (key, value) in values {
                    super::library::text(key, 128, false)?;
                    match value {
                        serde_json::Value::Bool(_) => {}
                        serde_json::Value::Number(n) if n.as_f64().is_some_and(f64::is_finite) => {}
                        serde_json::Value::String(s) => {
                            super::library::text(s, limits.max_state_string_bytes, true)?
                        }
                        _ => return Err(ErrorCode::InvalidMessage),
                    }
                }
            }
            overrides += instance.bone_overrides.len();
            if overrides > limits.max_bone_overrides {
                return Err(ErrorCode::InvalidMessage);
            }
            let mut paths = BTreeSet::new();
            for entry in &instance.bone_overrides {
                entry.bone.validate_with_limits(limits)?;
                entry.transform.validate_with_limits(limits)?;
                if entry.bone.instance_id != Some(instance.instance_id)
                    || !paths.insert(
                        serde_json::to_string(&entry.bone)
                            .map_err(|_| ErrorCode::InvalidMessage)?,
                    )
                {
                    return Err(ErrorCode::InvalidMessage);
                }
            }
            let parent = match &instance.anchor {
                AnchorRef::Standard { name } => {
                    if !STANDARD_ANCHORS.contains(&name.as_str()) {
                        return Err(ErrorCode::InvalidMessage);
                    }
                    None
                }
                AnchorRef::BaseBone { bone } => {
                    bone.validate_with_limits(limits)?;
                    if bone.instance_id.is_some() {
                        return Err(ErrorCode::InvalidMessage);
                    }
                    None
                }
                AnchorRef::AccessoryBone { bone } => {
                    bone.validate_with_limits(limits)?;
                    Some(bone.instance_id.ok_or(ErrorCode::InvalidMessage)?)
                }
                AnchorRef::Virtual { anchor_id } => {
                    virtuals
                        .get(anchor_id)
                        .ok_or(ErrorCode::InvalidMessage)?
                        .parent
                        .instance_id
                }
                AnchorRef::AutoTrail {
                    preferred_instance_id,
                } => *preferred_instance_id,
                AnchorRef::Unresolved { symbol } => {
                    super::library::text(symbol, 128, false)?;
                    None
                }
            };
            if let Some(parent) = parent {
                if !ids.contains(&parent) || parent == instance.instance_id {
                    return Err(ErrorCode::InvalidMessage);
                }
                edges.insert(instance.instance_id, parent);
            }
        }
        for id in &ids {
            let mut visited = BTreeSet::new();
            let mut next = Some(id);
            while let Some(id) = next {
                if !visited.insert(id) || visited.len() > limits.max_depth {
                    return Err(ErrorCode::InvalidMessage);
                }
                next = edges.get(id);
            }
        }
        Ok(())
    }

    pub fn dependencies(&self) -> Vec<&ResourceRef> {
        self.base
            .iter()
            .chain(self.instances.iter().map(|i| &i.resource))
            .collect()
    }
}

pub fn capabilities(limits: &OutfitLimits) -> serde_json::Value {
    let mut value = serde_json::to_value(limits).expect("validated outfit limits");
    let extension = serde_json::json!({
        "schema_version": 1, "configuration": true, "presets": true,
        "apply": true, "events": true, "publication": true, "standard_anchors": STANDARD_ANCHORS,
        "asset_distribution": asset_distribution_enabled(),
        "limits": limits
    });
    value
        .as_object_mut()
        .expect("limits object")
        .extend(extension.as_object().expect("capability object").clone());
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editable_bone_ranges_preserve_legacy_metadata_and_reject_invalid_declarations() {
        let legacy = serde_json::json!({
            "schema_version": 1, "category": "Wings", "cosmetic_anchor": "wing_anchor",
            "default_transform": {"position": [0.0,0.0,0.0], "rotation": [0.0,0.0,0.0], "scale": [1.0,1.0,1.0]},
            "supported_bases": ["FIGURA", "YSM"], "has_bones": true,
            "skeleton_revision": "one", "skeleton_hash": "a".repeat(64),
            "display_bones": true, "edit_bones": true, "exported_bones": [],
            "validation_source": "CLIENT_EXTRACTED"
        });
        let mut metadata: AccessoryMetadata = serde_json::from_value(legacy.clone()).unwrap();
        metadata.validate().unwrap();
        assert!(metadata.editable_bones.is_empty());
        assert_eq!(serde_json::to_value(&metadata).unwrap(), legacy);
        let bone = BoneRef {
            instance_id: None,
            path: vec![BoneSegment {
                name: "Cosmetic".into(),
                sibling_ordinal: 0,
            }],
            skeleton_hash: metadata.skeleton_hash.clone(),
        };
        metadata.editable_bones.push(bone.clone());
        metadata.validate().unwrap();
        let round_trip: AccessoryMetadata =
            serde_json::from_slice(&serde_json::to_vec(&metadata).unwrap()).unwrap();
        assert_eq!(round_trip, metadata);
        metadata.edit_bones = false;
        assert!(metadata.validate().is_err());
        metadata.edit_bones = true;
        metadata.editable_bones.push(bone.clone());
        assert!(metadata.validate().is_err());
        metadata.editable_bones.pop();
        metadata.editable_bones[0].instance_id = Some(Uuid::nil());
        assert!(metadata.validate().is_err());
        metadata.editable_bones[0] = bone.clone();
        metadata.editable_bones[0].skeleton_hash = "b".repeat(64);
        assert!(metadata.validate().is_err());
        metadata.editable_bones[0] = bone.clone();
        let mut limits = OutfitLimits::default();
        limits.max_resource_nodes = 0;
        assert!(metadata.validate_with_limits(&limits).is_err());
        limits = OutfitLimits::default();
        limits.max_depth = 1;
        metadata.editable_bones[0].path.push(BoneSegment {
            name: "child".into(),
            sibling_ordinal: 0,
        });
        assert!(metadata.validate_with_limits(&limits).is_err());
        metadata.editable_bones.clear();
        metadata.exported_bones = vec![bone.clone(), bone];
        assert!(metadata.validate().is_err());
    }

    fn fixture() -> OutfitConfig {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/outfit-v1/duplicate-instances.json"
        ))
        .unwrap()
    }

    #[test]
    fn exact_references_and_duplicate_instances_round_trip() {
        let config = fixture();
        config.validate().unwrap();
        assert_ne!(
            config.instances[0].instance_id,
            config.instances[1].instance_id
        );
        assert_eq!(config.instances[0].resource, config.instances[1].resource);
        let encoded = serde_json::to_vec(&config).unwrap();
        let decoded: OutfitConfig = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(config, decoded);
        let mut removed = decoded;
        removed.instances.remove(0);
        removed.validate().unwrap();
        assert_eq!(removed.instances[0].display_ordinal, 2);
        assert_eq!(*removed.display_ordinals.values().next().unwrap(), 2);
    }

    #[test]
    fn invalid_numbers_limits_owners_and_cycles_are_rejected() {
        let mut config = fixture();
        config.instances[0].transform.scale[0] = 0.0;
        assert!(config.validate().is_err());
        config.instances[0].transform.scale[0] = f64::NAN;
        assert!(config.validate().is_err());
        config = fixture();
        config.instances[1].instance_id = config.instances[0].instance_id;
        assert!(config.validate().is_err());
        config = fixture();
        config.instances[0].display_ordinal = 2;
        assert!(config.validate().is_err());
        config = fixture();
        let bone = |id| BoneRef {
            instance_id: Some(id),
            path: vec![BoneSegment {
                name: "same".into(),
                sibling_ordinal: 1,
            }],
            skeleton_hash: "a".repeat(64),
        };
        config.instances[0].anchor = AnchorRef::AccessoryBone {
            bone: bone(config.instances[1].instance_id),
        };
        config.instances[1].anchor = AnchorRef::AccessoryBone {
            bone: bone(config.instances[0].instance_id),
        };
        assert!(config.validate().is_err());
        config.instances[1].anchor = AnchorRef::BaseBone {
            bone: bone(config.instances[0].instance_id),
        };
        assert!(config.validate().is_err());
        config = fixture();
        config.instances[0].anchor = AnchorRef::AccessoryBone {
            bone: bone(Uuid::nil()),
        };
        assert!(config.validate().is_err());
        config = fixture();
        config.instances = vec![config.instances[0].clone(); MAX_INSTANCES + 1];
        assert!(config.validate().is_err());
    }

    #[test]
    fn unknown_types_and_path_ordinals_do_not_alias() {
        let json = include_str!("../../tests/fixtures/outfit-v1/duplicate-instances.json");
        assert!(
            serde_json::from_str::<OutfitConfig>(&json.replace("STANDARD", "UNKNOWN")).is_err()
        );
        assert!(serde_json::from_str::<OutfitConfig>(&json.replace("LOCAL", "UNKNOWN")).is_err());
        let a = BoneSegment {
            name: "same".into(),
            sibling_ordinal: 0,
        };
        let b = BoneSegment {
            name: "same".into(),
            sibling_ordinal: 1,
        };
        assert_ne!(a, b);
        assert!(serde_json::from_str::<ResourceKind>("\"UNKNOWN\"").is_err());
        let mut config = fixture();
        config.instances[0]
            .controls
            .insert("bad".into(), serde_json::json!({"nested": true}));
        assert!(config.validate().is_err());
    }
}
