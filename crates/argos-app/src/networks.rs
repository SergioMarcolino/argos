//! The Networks feature's UI: the sidebar picker and the manager window.
//!
//! Radmin exposes no membership, so a "network" here is a named list of IPs
//! the user maintains. The active name lives in the config; the set it maps to
//! lives in the shared [`Allowed`], which discovery and chat both consult.

use std::net::Ipv4Addr;

use eframe::egui::{self, RichText};

use argos_core::lan::{Allowed, Lan};

use crate::config::{self, AppConfig, NetworkProfile};
use crate::ui::theme;

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
    /// Why the last config write failed, shown in the manager so a silent
    /// revert on next launch has an explanation.
    save_error: Option<String>,
}

impl Networks {
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

    /// The sidebar combo above People. Returns the label the active network
    /// switched to, when it changed, so the caller can mark the moment.
    ///
    /// `busy` (sharing or watching) disables it: which peers exist decides who
    /// can connect, and changing that mid-stream would drop or keep peers
    /// against rules they connected under.
    pub fn picker(
        &mut self,
        ui: &mut egui::Ui,
        config: &mut AppConfig,
        allowed: &Allowed,
        busy: bool,
    ) -> Option<String> {
        let mut switched: Option<String> = None;
        let selected_label = if config.active_network.is_empty() {
            "All (no filter)".to_string()
        } else {
            config.active_network.clone()
        };
        ui.add_enabled_ui(!busy, |ui| {
            let response = egui::ComboBox::from_id_salt("network_picker")
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(config.active_network.is_empty(), "All (no filter)")
                        .clicked()
                    {
                        switched = Some(String::new());
                    }
                    for profile in &config.networks {
                        let name = profile.name.clone();
                        if ui
                            .selectable_label(config.active_network == name, &name)
                            .clicked()
                        {
                            switched = Some(name);
                        }
                    }
                    ui.separator();
                    if ui.button("Manage networks…").clicked() {
                        self.open = true;
                        ui.close();
                    }
                });
            if busy {
                response
                    .response
                    .on_disabled_hover_text("Stop sharing/watching to switch networks");
            }
        });
        if let Some(name) = &switched {
            config.active_network = name.clone();
            self.persist(config, allowed);
        }
        switched.map(|name| {
            if name.is_empty() {
                "All (no filter)".to_string()
            } else {
                name
            }
        })
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
                self.ip_list(ui, config, allowed);
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
            ui.horizontal(|ui| {
                if ui.selectable_label(self.selected == index, &name).clicked() {
                    self.selected = index;
                    self.error = None;
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
                if self.selected < config.networks.len() && ui.button("Rename").clicked() {
                    self.naming = Some(Some(self.selected));
                    self.name_input = config.networks[self.selected].name.clone();
                    self.error = None;
                }
                if self.selected < config.networks.len() && ui.button("Delete").clicked() {
                    let removed = config.networks.remove(self.selected);
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
    fn ip_list(&mut self, ui: &mut egui::Ui, config: &mut AppConfig, allowed: &Allowed) {
        let Some(profile) = config.networks.get(self.selected) else {
            ui.label(RichText::new("Select a profile to edit its IPs.").weak());
            return;
        };
        ui.label(RichText::new(format!("IPs in \"{}\"", profile.name)).strong());
        let mut changed = false;
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
