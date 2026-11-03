use std::path::PathBuf;
use std::process::Command;

const CANDIDATES: [&str; 2] = [
    r"C:\Program Files\Radmin VPN\Radmin VPN.exe",
    r"C:\Program Files (x86)\Radmin VPN\Radmin VPN.exe",
];

pub fn find_exe() -> Option<PathBuf> {
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

pub fn launch() -> Result<(), String> {
    if is_running() {
        return Ok(());
    }
    let exe = find_exe().ok_or_else(|| "Radmin VPN is not installed".to_string())?;
    Command::new(exe)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn is_running() -> bool {
    let output = match Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Radmin VPN.exe", "/NH"])
        .output()
    {
        Ok(output) => output,
        Err(_) => return false,
    };
    let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
    text.contains("radmin vpn")
}

// ---------------------------------------------------------------------------
// Radmin network discovery: service.log replay + registry count.
// ---------------------------------------------------------------------------

/// Which networks Radmin reports the user belongs to.
pub struct RadminNetworks {
    /// Joined network names, in join order, deduplicated.
    pub names: Vec<String>,
    /// Joined-network count from the registry, when the key could be read.
    pub registry_count: Option<usize>,
    /// Why the read failed, when it did.
    pub error: Option<String>,
}

const SERVICE_LOG_DIR: &str = r"C:\ProgramData\Famatech\Radmin VPN";

/// Replays `service.log` content into the list of currently joined networks.
///
/// The log is an append-only event stream, so "current" is the result of
/// folding every event in order: joins create (deduplicated), leaves delete,
/// creates count only on `Succeeded` for the same request id, renames move
/// the entry in place.
pub fn parse_networks(log: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut pending_creates: Vec<(String, String)> = Vec::new();
    for line in log.lines() {
        // Tab-separated timestamp, level, message — only the message matters.
        let message = line.rsplit('\t').next().unwrap_or(line);
        if let Some(name) = between(message, "You joined network '", "' as ") {
            push_unique(&mut names, name);
        } else if let Some(name) = message.strip_prefix("You joined gaming network '") {
            if let Some(name) = name.strip_suffix('\'') {
                push_unique(&mut names, name);
            }
        } else if let Some(rest) = message.strip_prefix("[req:") {
            if let Some(id_end) = rest.find(']') {
                let (id, rest) = (&rest[..id_end], &rest[id_end + 1..]);
                if let Some(name) = rest
                    .split("create network '")
                    .nth(1)
                    .and_then(|t| t.split('\'').next())
                {
                    pending_creates.push((id.to_string(), name.to_string()));
                } else if rest.trim() == "Succeeded" {
                    if let Some(pos) = pending_creates.iter().position(|(i, _)| i == id) {
                        let (_, name) = pending_creates.remove(pos);
                        push_unique(&mut names, &name);
                    }
                } else if rest.trim_start().starts_with("Failed") {
                    pending_creates.retain(|(i, _)| i != id);
                }
            }
        } else if let Some(name) = strip_quoted(message, "You left network '")
            .or_else(|| strip_quoted(message, "You deleted network '"))
        {
            names.retain(|n| n.as_str() != name);
        } else if let Some(name) = between(message, "Network '", "' was deleted") {
            names.retain(|n| n.as_str() != name);
        } else if let Some(name) = between(message, "You were kicked from network '", "' by ") {
            names.retain(|n| n.as_str() != name);
        } else if let Some(rest) = message.strip_prefix("Network '") {
            if let Some((old, new)) = rest.split_once("' was renamed to '") {
                if let Some(new) = new.strip_suffix('\'') {
                    if let Some(pos) = names.iter().position(|n| n == old) {
                        names[pos] = new.to_string();
                    }
                }
            }
        }
    }
    names
}

fn push_unique(names: &mut Vec<String>, name: &str) {
    if !name.is_empty() && !names.iter().any(|n| n == name) {
        names.push(name.to_string());
    }
}

/// The text between two fixed markers — safe for names containing quotes or
/// spaces, since only the markers delimit it.
fn between<'a>(text: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(prefix)?;
    rest.split_once(suffix).map(|(name, _)| name)
}

fn strip_quoted<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.strip_prefix(prefix)?.strip_suffix('\'')
}

/// Reads the current network list: rotated logs oldest-first, then the live
/// one, plus the registry's joined-network count.
pub fn joined_networks() -> RadminNetworks {
    let dir = std::path::Path::new(SERVICE_LOG_DIR);
    let mut logs: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("service") && n.contains(".log"))
            })
            .collect(),
        Err(error) => {
            return RadminNetworks {
                names: Vec::new(),
                registry_count: registry_count(),
                error: Some(format!("cannot read {SERVICE_LOG_DIR}: {error}")),
            }
        }
    };
    logs.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    if logs.is_empty() {
        return RadminNetworks {
            names: Vec::new(),
            registry_count: registry_count(),
            error: Some(format!("no service.log in {SERVICE_LOG_DIR}")),
        };
    }
    let mut text = String::new();
    for path in &logs {
        match read_utf16(path) {
            Ok(part) => {
                text.push_str(&part);
                text.push('\n');
            }
            Err(error) => {
                return RadminNetworks {
                    names: Vec::new(),
                    registry_count: registry_count(),
                    error: Some(format!("cannot read {}: {error}", path.display())),
                }
            }
        }
    }
    // One replay over the whole history: a join that lives only in a rotated
    // file must survive, and a leave must apply to events before it.
    let names = parse_networks(&text);
    RadminNetworks {
        names,
        registry_count: registry_count(),
        error: None,
    }
}

/// Radmin's service.log is UTF-16LE with a BOM.
fn read_utf16(path: &std::path::Path) -> Result<String, std::io::Error> {
    let bytes = std::fs::read(path)?;
    let units: Vec<u16> = (0..bytes.len() / 2)
        .map(|i| u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
        .collect();
    let text = String::from_utf16_lossy(&units);
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

/// Joined-network count from the registry: one GUID subkey per network under
/// the RadminVPN key. `None` when the key cannot be read.
fn registry_count() -> Option<usize> {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryInfoKeyW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
        KEY_WOW64_32KEY, REG_SAM_FLAGS,
    };
    unsafe {
        let mut key = HKEY::default();
        let sam = REG_SAM_FLAGS(KEY_READ.0 | KEY_WOW64_32KEY.0);
        if !RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\WOW6432Node\\Famatech\\RadminVPN\\1.0\\Networks"),
            None,
            sam,
            &mut key,
        )
        .is_ok()
        {
            return None;
        }
        let mut count = 0u32;
        let result = RegQueryInfoKeyW(
            key,
            None,
            None,
            None,
            Some(&mut count),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let _ = RegCloseKey(key);
        if result.is_ok() {
            Some(count as usize)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(level: &str, msg: &str) -> String {
        format!("2025.11.02 20:00:00.000\t{level}\t{msg}")
    }

    /// The replay is the feature's root of truth: every message shape Radmin
    /// writes has a case here, including the failed creates that must not
    /// become rooms.
    #[test]
    fn the_log_replays_into_current_memberships() {
        let log = [
            line("UPDATE", "You joined network 'friday-squad' as Member"),
            line("UPDATE", "You joined network 'game-night' as Member"),
            line("UPDATE", "You joined network 'The Cowboy Room' as Member"),
            line("UPDATE", "You joined network 'dup' as Member"),
            line("UPDATE", "You joined network 'dup' as Member"),
            line("INFO", "[req:7] Requested to create network 'made-it'"),
            line("INFO", "[req:7] Succeeded"),
            line("INFO", "[req:8] Requested to create network 'taken-name'"),
            line("INFO", "[req:8] Failed: name taken"),
            line("UPDATE", "You joined network 'short stay' as Member"),
            line("UPDATE", "You left network 'short stay'"),
            line("UPDATE", "You joined network 'old name' as Member"),
            line("UPDATE", "Network 'old name' was renamed to 'new name'"),
            line("UPDATE", "You joined network 'evicted' as Member"),
            line(
                "UPDATE",
                "You were kicked from network 'evicted' by 26.0.0.1",
            ),
            line("UPDATE", "You joined network 'Rafa's room' as Member"),
            line(
                "UPDATE",
                "You were kicked from network 'Rafa's room' by 26.0.0.1",
            ),
            line("UPDATE", "You joined network 'Rafa's place' as Member"),
            line("UPDATE", "You joined gaming network 'arcade'"),
            line("UPDATE", "Network 'arcade' was deleted by 26.0.0.9"),
            "junk line with no structure".to_string(),
        ]
        .join("\r\n");
        assert_eq!(
            parse_networks(&log),
            vec![
                "friday-squad",
                "game-night",
                "The Cowboy Room",
                "dup",
                "made-it",
                "new name",
                "Rafa's place",
            ]
        );
    }

    /// Rotated logs are one stream: a join that only the older file records
    /// must survive a leave of a different network in the newer one.
    #[test]
    fn rotated_logs_replay_as_a_single_stream() {
        let older = line("UPDATE", "You joined network 'kept' as Member");
        let newer = line("UPDATE", "You left network 'other'");
        let text = format!(
            "{older}
{newer}"
        );
        assert_eq!(parse_networks(&text), vec!["kept"]);
    }

    #[test]
    fn names_with_spaces_and_apostrophes_parse() {
        let log = line("UPDATE", "You joined network 'it\'s a me' as Member");
        assert_eq!(parse_networks(&log), vec!["it's a me"]);
    }
}
