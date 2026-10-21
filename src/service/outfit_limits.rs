//! Server-owned outfit budgets, advertised unchanged to clients within v1 safety bounds.
use super::outfit::{MAX_ANCHORS, MAX_DEPTH, MAX_INSTANCES, MAX_JSON_BYTES};
use crate::protocol::v1::ErrorCode;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct OutfitSettings {
    #[serde(deserialize_with = "read_limits")]
    pub limits: OutfitLimits,
}

fn read_limits<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<OutfitLimits, D::Error> {
    let limits = OutfitLimits::deserialize(deserializer)?;
    limits
        .validate()
        .map_err(|_| serde::de::Error::custom("invalid outfit limits"))?;
    Ok(limits)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct OutfitLimits {
    pub max_instances: usize,
    pub max_virtual_anchors: usize,
    pub max_depth: usize,
    pub max_resource_nodes: usize,
    pub max_total_nodes: usize,
    pub max_bone_overrides: usize,
    pub max_json_bytes: usize,
    pub max_trails: usize,
    pub max_trail_samples: usize,
    pub max_controls: usize,
    pub max_lua_state_entries: usize,
    pub max_state_string_bytes: usize,
    pub max_presets: usize,
    pub max_display_ordinals: usize,
    pub max_exported_bones: usize,
    pub position_limit: f64,
    pub rotation_limit: f64,
    pub min_scale: f64,
    pub max_scale: f64,
    pub dynamic_lighting_enabled: bool,
    pub max_dynamic_lights: usize,
    pub max_dynamic_light_level: u8,
    pub max_dynamic_light_radius: f64,
}

impl Default for OutfitLimits {
    fn default() -> Self {
        Self {
            max_instances: MAX_INSTANCES,
            max_virtual_anchors: MAX_ANCHORS,
            max_depth: MAX_DEPTH,
            max_resource_nodes: 8192,
            max_total_nodes: 32768,
            max_bone_overrides: 1024,
            max_json_bytes: MAX_JSON_BYTES,
            max_trails: 8,
            max_trail_samples: 256,
            max_controls: 128,
            max_lua_state_entries: 128,
            max_state_string_bytes: 512,
            max_presets: 64,
            max_display_ordinals: 256,
            max_exported_bones: 128,
            position_limit: 4096.0,
            rotation_limit: 36000.0,
            min_scale: 0.001,
            max_scale: 64.0,
            dynamic_lighting_enabled: true,
            max_dynamic_lights: 8,
            max_dynamic_light_level: 15,
            max_dynamic_light_radius: 16.0,
        }
    }
}

impl OutfitLimits {
    pub fn validate(&self) -> Result<(), ErrorCode> {
        let hard = Self::default();
        let bounded = [
            (self.max_instances, hard.max_instances),
            (self.max_virtual_anchors, hard.max_virtual_anchors),
            (self.max_resource_nodes, hard.max_resource_nodes),
            (self.max_total_nodes, hard.max_total_nodes),
            (self.max_bone_overrides, hard.max_bone_overrides),
            (self.max_trails, hard.max_trails),
            (self.max_controls, hard.max_controls),
            (self.max_lua_state_entries, hard.max_lua_state_entries),
            (self.max_state_string_bytes, hard.max_state_string_bytes),
            (self.max_presets, hard.max_presets),
            (self.max_display_ordinals, hard.max_display_ordinals),
            (self.max_exported_bones, hard.max_exported_bones),
        ];
        if bounded.iter().any(|(value, maximum)| value > maximum)
            || !(1..=hard.max_depth).contains(&self.max_depth)
            || !(256..=hard.max_json_bytes).contains(&self.max_json_bytes)
            || !(1..=hard.max_trail_samples).contains(&self.max_trail_samples)
            || !self.position_limit.is_finite()
            || !(0.0..=hard.position_limit).contains(&self.position_limit)
            || !self.rotation_limit.is_finite()
            || !(0.0..=hard.rotation_limit).contains(&self.rotation_limit)
            || !self.min_scale.is_finite()
            || !self.max_scale.is_finite()
            || self.min_scale < hard.min_scale
            || self.max_scale > hard.max_scale
            || self.max_scale < self.min_scale
            || self.max_dynamic_lights > 32
            || self.max_dynamic_light_level > 15
            || !self.max_dynamic_light_radius.is_finite()
            || !(0.0..=32.0).contains(&self.max_dynamic_light_radius)
        {
            return Err(ErrorCode::InvalidMessage);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_settings_default_and_invalid_values_are_rejected() {
        let settings: OutfitSettings =
            toml::from_str("[limits]\nmax_instances = 3\nmax_trails = 0\n").unwrap();
        assert_eq!(settings.limits.max_instances, 3);
        assert_eq!(settings.limits.max_trails, 0);
        assert_eq!(settings.limits.max_depth, 64);
        for value in [
            "max_instances = 33",
            "max_depth = 0",
            "max_json_bytes = 65537",
            "position_limit = nan",
            "min_scale = 0.0",
            "max_scale = 0.0001",
            "unknown_limit = 1",
            "max_dynamic_lights = 33",
            "max_dynamic_light_level = 16",
            "max_dynamic_light_radius = nan",
        ] {
            assert!(toml::from_str::<OutfitSettings>(&format!("[limits]\n{value}\n")).is_err());
        }
    }
}
