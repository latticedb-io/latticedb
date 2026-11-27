//! Predefined profiles (SPEC-02 §10): `dev`, `laptop`, `server`, `analytics`.
//! Shipped as versioned TOML sets embedded at build time; defaults never live
//! in code literals.

use super::{ConfigError, HostConfig, parse_config};

/// Profile-set version; bump on any semantic change to the shipped sets.
pub const PROFILE_VERSION: u32 = 1;

pub const DEV_TOML: &str = include_str!("../../profiles/dev.toml");
pub const LAPTOP_TOML: &str = include_str!("../../profiles/laptop.toml");
pub const SERVER_TOML: &str = include_str!("../../profiles/server.toml");
pub const ANALYTICS_TOML: &str = include_str!("../../profiles/analytics.toml");

/// Load and validate a profile by name (`dev` | `laptop` | `server` | `analytics`).
pub fn load_profile(name: &str) -> Result<HostConfig, ConfigError> {
    let text = match name {
        "dev" => DEV_TOML,
        "laptop" => LAPTOP_TOML,
        "server" => SERVER_TOML,
        "analytics" => ANALYTICS_TOML,
        other => {
            return Err(ConfigError {
                message: format!(
                    "unknown profile `{other}`; available: dev, laptop, server, analytics"
                ),
            });
        }
    };
    parse_config(text)
}
