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
    /// The room mirrors a Radmin network of the same name: membership comes
    /// from peers' hellos rather than the manual list alone.
    #[serde(default)]
    pub radmin: bool,
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
    /// Auto rooms the user deleted by name — the sync must not recreate them.
    #[serde(default)]
    pub dismissed_radmin: Vec<String>,
    /// The channel sidebar folded away (rail only).
    #[serde(default)]
    pub sidebar_collapsed: bool,
}

/// Reconciles the profile list with what Radmin reports: a room for every
/// Radmin network (unless dismissed), stale auto rooms removed, manual rooms
/// untouched. Returns the new list and whether it changed.
///
/// `keep` is the name of an auto room that must survive this round even if
/// Radmin dropped it — the caller defers removal while it is in use.
pub fn merge_radmin(
    profiles: &[NetworkProfile],
    dismissed: &[String],
    radmin_names: &[String],
    keep: Option<&str>,
) -> (Vec<NetworkProfile>, bool) {
    let mut out = profiles.to_vec();
    let mut changed = false;
    // Drop auto rooms Radmin no longer reports (manual rooms are never
    // touched by the sync).
    out.retain(|profile| {
        let stale = profile.radmin
            && !radmin_names.contains(&profile.name)
            && keep != Some(profile.name.as_str());
        changed |= stale;
        !stale
    });
    for name in radmin_names {
        if dismissed.contains(name) {
            continue;
        }
        match out.iter_mut().find(|profile| profile.name == *name) {
            Some(profile) if !profile.radmin => {
                profile.radmin = true;
                changed = true;
            }
            Some(_) => {}
            None => {
                out.push(NetworkProfile {
                    name: name.clone(),
                    ips: Vec::new(),
                    radmin: true,
                });
                changed = true;
            }
        }
    }
    (out, changed)
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
                    ..Default::default()
                },
                NetworkProfile {
                    name: "empty".to_string(),
                    ips: Vec::new(),
                    ..Default::default()
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

    /// The merge is the sync's whole logic: rooms appear for new Radmin
    /// networks, vanish when Radmin drops them, and manual rooms survive.
    #[test]
    fn radmin_rooms_merge_without_disturbing_manual_ones() {
        let profiles = vec![
            NetworkProfile {
                name: "manual".to_string(),
                ips: vec!["26.0.0.1".to_string()],
                radmin: false,
            },
            NetworkProfile {
                name: "stale".to_string(),
                ips: Vec::new(),
                radmin: true,
            },
        ];
        let names = vec!["manual".to_string(), "fresh".to_string()];
        let (out, changed) = merge_radmin(&profiles, &[], &names, None);
        assert!(changed);
        // "stale" dropped, "fresh" added as radmin, manual untouched.
        assert_eq!(
            out.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            vec!["manual", "fresh"]
        );
        assert!(out[1].radmin);
        // The manual profile gains the radmin flag, keeps its IPs.
        assert!(out[0].radmin && out[0].ips == vec!["26.0.0.1".to_string()]);
        // Idempotent: the same sync again changes nothing.
        let (_, changed) = merge_radmin(&out, &[], &names, None);
        assert!(!changed);
    }

    #[test]
    fn dismissed_rooms_are_not_recreated_and_keep_defers_removal() {
        let names = vec!["gone".to_string()];
        let (out, _) = merge_radmin(&[], &["gone".to_string()], &names, None);
        assert!(out.is_empty());
        // An in-use stale room survives one more round.
        let stale = vec![NetworkProfile {
            name: "stale".to_string(),
            ips: Vec::new(),
            radmin: true,
        }];
        let (out, changed) = merge_radmin(&stale, &[], &[], Some("stale"));
        assert_eq!(out.len(), 1);
        assert!(!changed);
    }
}
