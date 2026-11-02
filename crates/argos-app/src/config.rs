use std::collections::HashSet;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

fn default_volume_percent() -> u32 {
    100
}

fn default_hardware_encoder() -> bool {
    true
}

/// A named list of Radmin IPs: one of the user's Radmin "networks".
///
/// Radmin puts every joined network on a single adapter, so there is no
/// membership to discover — the user names the groups and lists the addresses
/// that belong to each.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkProfile {
    pub name: String,
    #[serde(default)]
    pub ips: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub name: String,
    #[serde(default = "default_volume_percent")]
    pub volume_percent: u32,
    #[serde(default = "default_hardware_encoder")]
    pub hardware_encoder: bool,
    /// The network profiles the user has defined. Empty for every install that
    /// predates the feature; `serde(default)` keeps that file loading.
    #[serde(default)]
    pub networks: Vec<NetworkProfile>,
    /// The active profile's name; empty means no filter ("All").
    #[serde(default)]
    pub active_network: String,
}

impl AppConfig {
    /// The addresses the active profile admits, or `None` for "no filter".
    ///
    /// `None` covers both "no profile selected" and "the selected name no
    /// longer exists" — a stale selection must fail open (everyone visible)
    /// rather than closed (nobody visible), because the profile it points at
    /// was deleted, not the whole world. `Some` with an empty set is the
    /// opposite case: a real profile with no IPs yet, which admits nobody
    /// until one is added.
    pub fn allowed_ips(&self) -> Option<HashSet<Ipv4Addr>> {
        if self.active_network.is_empty() {
            return None;
        }
        let profile = self
            .networks
            .iter()
            .find(|profile| profile.name == self.active_network)?;
        Some(
            profile
                .ips
                .iter()
                .filter_map(|ip| ip.parse::<Ipv4Addr>().ok())
                .collect(),
        )
    }
}

pub fn load() -> AppConfig {
    confy::load("argos", None).unwrap_or_default()
}

/// Writes the profile, or says why it could not.
///
/// The error used to be discarded, which made a failed save invisible: the user
/// changes a setting, watches it take effect, and finds it reverted on the next
/// launch with nothing to explain it. The likeliest cause is a config directory
/// that cannot be written — a read-only or redirected roaming profile.
pub fn save(config: &AppConfig) -> Result<(), String> {
    confy::store("argos", None, config).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config file written before networks existed has none of the keys.
    /// It must still load — with no profiles and no active network — or every
    /// existing install would lose its settings on upgrade.
    #[test]
    fn an_old_config_loads_with_network_defaults() {
        let old = "name = \"desk\"\nvolume_percent = 80\nhardware_encoder = false\n";
        let config: AppConfig = toml::from_str(old).expect("old config must parse");
        assert_eq!(config.name, "desk");
        assert!(config.networks.is_empty());
        assert!(config.active_network.is_empty());
        assert!(config.allowed_ips().is_none());
    }

    /// The three states of the filter: nobody selected (all), a named profile
    /// (only its IPs), and a selected name with no profile behind it (all —
    /// fail open, not closed).
    #[test]
    fn allowed_ips_maps_the_active_profile_to_a_set() {
        let mut config = AppConfig {
            networks: vec![
                NetworkProfile {
                    name: "home".to_string(),
                    ips: vec!["26.0.0.7".to_string(), "not-an-ip".to_string()],
                },
                NetworkProfile {
                    name: "empty".to_string(),
                    ips: Vec::new(),
                },
            ],
            ..Default::default()
        };

        assert!(config.allowed_ips().is_none());

        config.active_network = "home".to_string();
        let set = config.allowed_ips().expect("a profile exists");
        // The unparseable entry is skipped, not fatal.
        assert_eq!(set.len(), 1);
        assert!(set.contains(&Ipv4Addr::new(26, 0, 0, 7)));

        config.active_network = "empty".to_string();
        assert_eq!(config.allowed_ips(), Some(HashSet::new()));

        config.active_network = "deleted".to_string();
        assert!(config.allowed_ips().is_none());
    }
}
