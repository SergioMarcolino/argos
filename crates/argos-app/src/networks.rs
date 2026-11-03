//! The Networks feature's UI: the sidebar picker and the manager window.
//!
//! Radmin exposes no membership, so a "network" here is a named list of IPs
//! the user maintains. The active name lives in the config; the set it maps to
//! lives in the shared [`Allowed`], which discovery and chat both consult.

use std::collections::HashSet;
use std::net::Ipv4Addr;
use std::time::Instant;

use eframe::egui::{self, RichText};

use argos_core::lan::{network_tag, Allowed, Lan, LanPeer};

use crate::config::{self, merge_radmin, AppConfig, NetworkProfile};
use crate::radmin::{self, RadminNetworks};
use crate::theme;

/// The picker/manager state. Editing happens straight on `config.networks` and
/// every change is persisted immediately — there is no draft to lose.
#[derive(Default)]
pub struct Networks {
    open: bool,
    /// Which profile the manager is editing, by index into `config.networks`.
    selected: usize,
    /// Some = a name is being typed, either for a new profile or a rename of
    /// the index it carries.
    naming: Option<Option<usize>>,
    name_input: String,
    ip_input: String,
    /// Validation feedback inside the manager (bad name, bad IP, duplicate).
    error: Option<String>,
    /// A running Radmin sync: the worker reports over this channel.
    sync_rx: Option<std::sync::mpsc::Receiver<RadminNetworks>>,
    /// The last sync's outcome, for the manager's status line.
    radmin_status: Option<String>,
    /// When the last sync started — the 30 s cadence is timed from it. `None`
    /// means never, which makes the first `drain_sync` the startup sync.
    last_sync: Option<Instant>,
    /// Why the last config write failed, shown in the manager so a silent
    /// revert on next launch has an explanation.
    save_error: Option<String>,
}

impl Networks {
    /// The addresses a room admits: its manual list plus every online peer
    /// whose hello advertises this network's tag. Manual rooms keep exactly
    /// their list — the `radmin` flag is the whole switch.
    pub fn room_allowed(profile: &NetworkProfile, peers: &[LanPeer]) -> HashSet<Ipv4Addr> {
        let mut set: HashSet<Ipv4Addr> = profile
            .ips
            .iter()
            .filter_map(|ip| ip.parse().ok())
            .collect();
        if profile.radmin {
            let tag = network_tag(&profile.name);
            for peer in peers {
                if peer.radmin_networks.contains(&tag) {
                    if let std::net::IpAddr::V4(ip) = peer.addr.ip() {
                        set.insert(ip);
                    }
                }
            }
        }
        set
    }

    /// Kicks off a Radmin read on a background thread — the log and registry
    /// reads are disk IO and must never stall the UI.
    pub fn sync_now(&mut self) {
        if self.sync_rx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            self.sync_rx = Some(rx);
            std::thread::spawn(move || {
                let _ = tx.send(radmin::joined_networks());
            });
        }
        self.last_sync = Some(Instant::now());
    }

    /// Applies whatever a finished sync produced: merges rooms into the
    /// config, advertises the new tag list, returns whether the config
    /// changed (caller saves + refreshes the filter).
    ///
    /// `busy` defers removal of the active room to the next sync — dropping
    /// the room out from under a live stream would change who may connect.
    pub fn drain_sync(
        &mut self,
        config: &mut AppConfig,
        lan: Option<&argos_core::lan::Lan>,
        busy: bool,
    ) -> bool {
        if self
            .last_sync
            .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(30))
        {
            self.sync_now();
        }
        let Some(result) = self.sync_rx.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return false;
        };
        self.sync_rx = None;
        if let Some(lan) = lan {
            lan.set_radmin_networks(result.names.iter().map(|n| network_tag(n)).collect());
        }
        self.radmin_status = match (&result.error, result.registry_count) {
            (Some(error), _) => Some(error.clone()),
            (None, Some(count)) if count != result.names.len() => Some(format!(
                "Radmin reports {count} networks but its log shows {} — check your rooms.",
                result.names.len()
            )),
            (None, _) => Some(format!("Found {} Radmin networks", result.names.len())),
        };
        let keep = if busy && !config.active_network.is_empty() {
            Some(config.active_network.as_str())
        } else {
            None
        };
        let (merged, changed) = merge_radmin(
            &config.networks,
            &config.dismissed_radmin,
            &result.names,
            keep,
        );
        if changed {
            config.networks = merged;
            // A removed active room only happens when !busy (keep covers it).
            if !config
                .networks
                .iter()
                .any(|p| p.name == config.active_network)
            {
                config.active_network = String::new();
            }
        }
        changed
    }

    /// Opens the manager window (the rail's "+" and the sidebar's gear use it).
    pub fn open_manager(&mut self) {
        self.open = true;
    }

    /// Switches the active network by name ("" = All), persists, and returns
    /// the label the chat system line should use.
    pub fn select(&mut self, config: &mut AppConfig, allowed: &Allowed, name: String) -> String {
        config.active_network = name;
        self.persist(config, allowed);
        if config.active_network.is_empty() {
            "All (no filter)".to_string()
        } else {
            config.active_network.clone()
        }
    }

    /// Writes the config and refreshes the shared filter, reporting a failed
    /// write into `save_error` rather than letting it revert silently.
    fn persist(&mut self, config: &AppConfig, allowed: &Allowed) {
        match config::save(config) {
            Ok(()) => self.save_error = None,
            Err(error) => self.save_error = Some(format!("not saved: {error}")),
        }
        if let Ok(mut set) = allowed.write() {
            *set = config.allowed_ips();
        }
    }

    /// The manager window: profiles, their IPs, and who the VPN can see.
    pub fn window(
        &mut self,
        ctx: &egui::Context,
        config: &mut AppConfig,
        allowed: &Allowed,
        lan: Option<&Lan>,
    ) {
        if !self.open {
            return;
        }
        let mut open = self.open;
        egui::Window::new("Networks")
            .open(&mut open)
            .collapsible(false)
            .default_width(360.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if let Some(error) = &self.save_error {
                    ui.label(RichText::new(error).color(theme::ERR));
                }
                self.profile_list(ui, config, allowed);
                ui.separator();
                self.ip_list(ui, config, allowed, lan);
                ui.separator();
                self.seen_list(ui, config, allowed, lan);
                match lan.and_then(|lan| lan.local_address()) {
                    Some(ip) => ui.label(RichText::new(format!("This PC: {ip}")).weak()),
                    None => ui.label(RichText::new("This PC: no Radmin address yet").weak()),
                };
            });
        self.open = open;
    }

    /// The list of profiles and the New/Rename/Delete row beneath it.
    fn profile_list(&mut self, ui: &mut egui::Ui, config: &mut AppConfig, allowed: &Allowed) {
        ui.label(RichText::new("Profiles").strong());
        if config.networks.is_empty() {
            ui.label(RichText::new("No profiles yet — create one to filter by network.").weak());
        }
        let mut changed = false;
        let count = config.networks.len();
        for index in 0..count {
            let name = config.networks[index].name.clone();
            let radmin_room = config.networks.get(index).is_some_and(|p| p.radmin);
            ui.horizontal(|ui| {
                if ui.selectable_label(self.selected == index, &name).clicked() {
                    self.selected = index;
                    self.error = None;
                }
                if radmin_room {
                    ui.label(RichText::new("from Radmin").weak());
                }
                if config.active_network == name {
                    ui.label(RichText::new("active").weak());
                }
            });
        }
        // Naming UI: one row shared by New and Rename.
        if let Some(which) = self.naming {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.name_input)
                        .hint_text("Network name")
                        .desired_width(160.0),
                );
                if ui.button("Save").clicked() {
                    let name = self.name_input.trim().to_string();
                    match self.validate_name(config, &name, which) {
                        Err(error) => self.error = Some(error),
                        Ok(()) => match which {
                            None => {
                                config.networks.push(NetworkProfile {
                                    name: name.clone(),
                                    ips: Vec::new(),
                                    radmin: false,
                                });
                                self.selected = config.networks.len() - 1;
                                self.naming = None;
                                changed = true;
                            }
                            Some(index) => {
                                // Keep `active_network` pointing at the same
                                // profile across the rename.
                                if config.networks[index].name == config.active_network {
                                    config.active_network = name.clone();
                                }
                                config.networks[index].name = name;
                                self.naming = None;
                                changed = true;
                            }
                        },
                    }
                }
                if ui.button("Cancel").clicked() {
                    self.naming = None;
                    self.error = None;
                }
            });
        } else {
            ui.horizontal(|ui| {
                if ui.button("New").clicked() {
                    self.naming = Some(None);
                    self.name_input.clear();
                    self.error = None;
                }
                // An auto room's name is the link to the Radmin network;
                // renaming it would cut the link, so Rename is disabled.
                let can_rename =
                    self.selected < config.networks.len() && !config.networks[self.selected].radmin;
                if ui
                    .add_enabled(can_rename, egui::Button::new("Rename"))
                    .clicked()
                {
                    self.naming = Some(Some(self.selected));
                    self.name_input = config.networks[self.selected].name.clone();
                    self.error = None;
                }
                if self.selected < config.networks.len() && ui.button("Delete").clicked() {
                    let removed = config.networks.remove(self.selected);
                    if removed.radmin && !config.dismissed_radmin.contains(&removed.name) {
                        config.dismissed_radmin.push(removed.name.clone());
                    }
                    if removed.name == config.active_network {
                        config.active_network.clear();
                    }
                    if self.selected >= config.networks.len() && self.selected > 0 {
                        self.selected -= 1;
                    }
                    changed = true;
                }
            });
        }
        if changed {
            self.persist(config, allowed);
        }
    }

    fn validate_name(
        &self,
        config: &AppConfig,
        name: &str,
        excluding: Option<usize>,
    ) -> Result<(), String> {
        if name.is_empty() {
            return Err("Name cannot be empty".to_string());
        }
        let duplicate = config
            .networks
            .iter()
            .enumerate()
            .any(|(index, profile)| Some(index) != excluding && profile.name == name);
        if duplicate {
            return Err(format!("\"{name}\" already exists"));
        }
        Ok(())
    }

    /// The selected profile's IP list with its add/remove controls.
    fn ip_list(
        &mut self,
        ui: &mut egui::Ui,
        config: &mut AppConfig,
        allowed: &Allowed,
        lan: Option<&Lan>,
    ) {
        let Some(profile) = config.networks.get(self.selected) else {
            ui.label(RichText::new("Select a profile to edit its IPs.").weak());
            return;
        };
        ui.label(RichText::new(format!("IPs in \"{}\"", profile.name)).strong());
        let mut changed = false;
        // Auto room members come from peers' hellos, not the manual list.
        if let Some(profile) = config.networks.get(self.selected) {
            if profile.radmin {
                let tag = network_tag(&profile.name);
                let found: Vec<String> = lan
                    .map(|lan| {
                        lan.all_peers()
                            .iter()
                            .filter(|p| p.radmin_networks.contains(&tag))
                            .map(|p| p.name.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                if found.is_empty() {
                    ui.label(RichText::new("Nobody online with Argos 1.5.1+ yet").weak());
                } else {
                    ui.label(
                        RichText::new(format!("Found automatically: {}", found.join(", "))).weak(),
                    );
                }
            }
        }
        let mut remove: Option<usize> = None;
        let count = config.networks[self.selected].ips.len();
        for index in 0..count {
            ui.horizontal(|ui| {
                ui.label(&config.networks[self.selected].ips[index]);
                if ui.small_button("×").clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            config.networks[self.selected].ips.remove(index);
            changed = true;
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.ip_input)
                    .hint_text("26.x.x.x")
                    .desired_width(120.0),
            );
            if ui.button("Add IP").clicked() {
                match Self::add_ip(&mut config.networks[self.selected], &self.ip_input) {
                    Ok(()) => {
                        self.ip_input.clear();
                        self.error = None;
                        changed = true;
                    }
                    Err(error) => self.error = Some(error),
                }
            }
        });
        if changed {
            self.persist(config, allowed);
        }
    }

    fn add_ip(profile: &mut NetworkProfile, input: &str) -> Result<(), String> {
        let text = input.trim();
        text.parse::<Ipv4Addr>()
            .map_err(|_| format!("\"{text}\" is not an IPv4 address"))?;
        if profile.ips.iter().any(|ip| ip == text) {
            return Err(format!("{text} is already in the list"));
        }
        profile.ips.push(text.to_string());
        Ok(())
    }

    /// Everyone the VPN currently hears, with a shortcut into the profile.
    ///
    /// This is the unfiltered peer list on purpose: it is the one place a
    /// filtered-out peer is still visible, because the user cannot add an
    /// address they have never seen.
    fn seen_list(
        &mut self,
        ui: &mut egui::Ui,
        config: &mut AppConfig,
        allowed: &Allowed,
        lan: Option<&Lan>,
    ) {
        ui.label(RichText::new("Seen on the VPN").strong());
        let peers = lan.map(|lan| lan.all_peers()).unwrap_or_default();
        if peers.is_empty() {
            ui.label(RichText::new("Nobody else is visible right now.").weak());
        }
        let mut changed = false;
        for peer in peers {
            let Some(ip) = (match peer.addr {
                std::net::SocketAddr::V4(v4) => Some(*v4.ip()),
                std::net::SocketAddr::V6(_) => None,
            }) else {
                continue;
            };
            ui.horizontal(|ui| {
                ui.label(format!("{} — {ip}", peer.name));
                let already = config.networks.get(self.selected).is_some_and(|profile| {
                    profile.ips.iter().any(|entry| entry == &ip.to_string())
                });
                if !already
                    && self.selected < config.networks.len()
                    && ui.small_button("Add").clicked()
                {
                    config.networks[self.selected].ips.push(ip.to_string());
                    changed = true;
                }
            });
        }
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(theme::ERR));
        }
        if changed {
            self.persist(config, allowed);
        }
    }
}
