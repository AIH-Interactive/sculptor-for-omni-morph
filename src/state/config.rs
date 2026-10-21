use std::{collections::HashMap, io::Read, path::PathBuf};

use serde::Deserialize;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::auth::{default_authproviders, AuthProviders, Userinfo};

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub listen: String,
    pub token: Option<String>,
    pub assets_updater_enabled: bool,
    #[serde(default = "protobuf_default")]
    pub protobuf_enabled: bool,
    pub motd: CMotd,
    #[serde(default = "default_authproviders")]
    pub auth_providers: AuthProviders,
    pub limitations: Limitations,
    #[serde(default)]
    pub mc_folder: PathBuf,
    #[serde(default)]
    pub advanced_users: HashMap<Uuid, AdvancedUsers>,
    #[serde(default, rename = "model_library")]
    pub model_library: crate::service::library_admin::LibraryConfig,
    #[serde(default)]
    pub outfit: crate::service::outfit_limits::OutfitSettings,
}

fn protobuf_default() -> bool {
    true
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CMotd {
    pub display_server_info: bool,
    pub custom_text: String,
    #[serde(rename = "sInfoUptime")]
    pub text_uptime: String,
    #[serde(rename = "sInfoAuthClients")]
    pub text_authclients: String,
    #[serde(rename = "sInfoDrawIndent")]
    pub draw_indent: bool,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Limitations {
    pub max_avatar_size: u64,
    pub max_avatars: u64,
    pub can_upload: bool,
    #[serde(default = "default_package_nodes", deserialize_with = "package_nodes")]
    pub max_package_nodes: i64,
}

fn default_package_nodes() -> i64 {
    262_144
}

fn package_nodes<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    let value = i64::deserialize(deserializer)?;
    if value < -1 {
        return Err(serde::de::Error::custom(
            "maxPackageNodes must be -1 or nonnegative",
        ));
    }
    Ok(value)
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AdvancedUsers {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub banned: bool,
    #[serde(default)]
    pub special: [u8;6],
    #[serde(default)]
    pub pride: [u8;25],
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BannedPlayer {
    pub uuid: Uuid,
    pub name: String,
}

impl From<BannedPlayer> for Userinfo {
    fn from(val: BannedPlayer) -> Self {
        Userinfo {
            uuid: val.uuid,
            nickname: val.name,
            banned: true,
            ..Default::default()
        }
    }
}

impl Config {
    pub fn parse(path: PathBuf) -> Self {
        let mut file = std::fs::File::open(path).expect("Access denied or file doesn't exists!");
        let mut data = String::new();
        file.read_to_string(&mut data).unwrap();

        toml::from_str(&data).unwrap_or_else(|error| {
            tracing::error!("{}", configuration_error_summary(&error));
            panic!("Invalid configuration; see log messages for details")
        })
    }

    pub fn verify_token(&self, suspicious: &str) -> crate::ApiResult<()> {
        use crate::ApiError;
        match &self.token {
            Some(token) => {
                if token == suspicious {
                    debug!("Admin token passed!");
                    Ok(())
                } else {
                    warn!("Unknown tryed to use admin functions, but use wrong token!");
                    Err(ApiError::Unauthorized)
                }
            },
            None => {
                warn!("Unknown tryed to use admin functions, but token is not defined!");
                Err(ApiError::BadRequest)
            },
        }
    }
}

fn configuration_error_summary(error: &toml::de::Error) -> String {
    // Both Debug/Display and arbitrary deserializer messages can contain secrets.
    // Only retain validation messages that we define as fixed strings ourselves.
    let reason = match error.message() {
        "invalid model library administrator identity" => {
            "invalid model library administrator identity"
        }
        "duplicate model library administrator identity" => {
            "duplicate model library administrator identity"
        }
        "invalid model library administrator password hash" => {
            "invalid model library administrator password hash"
        }
        _ => "invalid TOML syntax or configuration value",
    };
    match error.span() {
        Some(span) => format!(
            "Invalid configuration at bytes {}..{}: {reason}",
            span.start, span.end
        ),
        None => format!("Invalid configuration: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outfit_limits_have_their_own_nested_config_section() {
        let example = include_str!("../../Config.example.toml");
        let config: Config = toml::from_str(example).unwrap();
        assert_eq!(config.outfit.limits.max_instances, 32);
        let config: Config =
            toml::from_str(&example.replace("max_instances = 32", "max_instances = 4")).unwrap();
        assert_eq!(config.outfit.limits.max_instances, 4);
        assert!(
            toml::from_str::<Config>(&example.replace("max_instances = 32", "max_instances = 33"))
                .is_err()
        );
    }

    #[test]
    fn package_node_limit_defaults_and_unlimited_setting() {
        let base = "maxAvatarSize = 102400\nmaxAvatars = 100\ncanUpload = true\n";
        let defaults: Limitations = toml::from_str(base).unwrap();
        assert_eq!(defaults.max_package_nodes, 262_144);
        for value in [-1, 0, 500_000] {
            let settings: Limitations =
                toml::from_str(&format!("{base}maxPackageNodes = {value}\n")).unwrap();
            assert_eq!(settings.max_package_nodes, value);
        }
        assert!(toml::from_str::<Limitations>(&format!("{base}maxPackageNodes = -2\n")).is_err());
    }

    #[test]
    fn configuration_error_does_not_disclose_input_or_deserializer_values() {
        for input in [
            "token = [\"synthetic-config-secret\"]\n",
            "token = \"synthetic-config-secret\"\ninvalid = [",
        ] {
            let error = toml::from_str::<Config>(input).unwrap_err();
            let summary = configuration_error_summary(&error);
            assert!(summary.contains("Invalid configuration"));
            assert!(summary.contains("at bytes"));
            assert!(!summary.contains("synthetic-config-secret"));
            assert!(!summary.contains("token"));
        }
    }
}
