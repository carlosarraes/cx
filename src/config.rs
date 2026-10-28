use anyhow::{bail, Context, Result};
use serde::Deserialize;
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub usage_max_age_seconds: i64,
    pub affinity_seconds: i64,
    pub max_headless_per_account: usize,
}
impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            usage_max_age_seconds: 300,
            affinity_seconds: 3600,
            max_headless_per_account: 2,
        }
    }
}
impl RoutingConfig {
    pub fn load() -> Result<Self> {
        #[derive(Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        struct Config {
            routing: RoutingConfig,
        }
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or(
                dirs::home_dir()
                    .context("cannot locate home")?
                    .join(".config"),
            );
        let config = match std::fs::read_to_string(root.join("cx/config.toml")) {
            Ok(text) => {
                toml::from_str::<Config>(&text)
                    .map_err(|_| anyhow::anyhow!("invalid cx routing configuration"))?
                    .routing
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        if !(1..=3600).contains(&config.usage_max_age_seconds)
            || !(0..=86400).contains(&config.affinity_seconds)
            || !(1..=1000).contains(&config.max_headless_per_account)
        {
            bail!("invalid routing limits: usage age 1..3600s, affinity 0..86400s, headless cap 1..1000");
        }
        Ok(config)
    }
}
