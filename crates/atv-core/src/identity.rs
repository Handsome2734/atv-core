//! Device identity management: per-device UUID, hardware MAC, device name,
//! and persistent private keys.
//!
//! Ensures that each device on the network advertises a unique name and ID
//! to iOS Apple TV Remote, while maintaining persistent identity across restarts.

use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::server::AtvConfig;

pub const DEFAULT_FALLBACK_NAME: &str = "Mac Remote";

/// Full device identity used for Bonjour advertisement and Companion Link pairing.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    /// Device name displayed in iOS Control Center / Apple TV Remote
    pub name: String,
    /// Hardware / MAC-like identifier (e.g. "84:2F:57:2E:6C:EE")
    pub device_id: String,
    /// 6-byte raw MAC address
    pub mac_bytes: [u8; 6],
    /// Server identifier UUID (e.g. "F232C260-003A-5396-99D1-F8164BB03DF2")
    pub server_identifier: String,
    /// 32-character hex unique identifier (UUID without hyphens)
    pub unique_id: String,
    /// 32-byte persistent private key seed for Ed25519, X25519, and SRP-6a
    pub private_key: [u8; 32],
}

impl DeviceIdentity {
    /// Auto-detect the identity of the current machine.
    pub fn detect() -> Self {
        let name = detect_computer_name();
        let server_identifier = detect_hardware_uuid().unwrap_or_else(|| {
            load_or_generate_persistent_uuid()
        });
        let (device_id, mac_bytes) = detect_mac_address(&server_identifier);
        let unique_id = server_identifier.replace('-', "").to_uppercase();
        let private_key = derive_private_key(&server_identifier);

        Self {
            name,
            device_id,
            mac_bytes,
            server_identifier,
            unique_id,
            private_key,
        }
    }

    /// Resolve identity from `AtvConfig`, filling any missing or empty fields
    /// with the auto-detected per-device identity.
    pub fn from_config(config: &AtvConfig) -> Self {
        let detected = Self::detect();

        let name = if config.name.trim().is_empty() {
            detected.name
        } else {
            config.name.clone()
        };

        let server_identifier = match &config.server_identifier {
            Some(id) if !id.trim().is_empty() => id.trim().to_uppercase(),
            _ => detected.server_identifier,
        };

        let (device_id, mac_bytes) = match &config.device_id {
            Some(dev_id) if !dev_id.trim().is_empty() => {
                let trimmed = dev_id.trim();
                let bytes = parse_mac_address(trimmed).unwrap_or(detected.mac_bytes);
                (trimmed.to_uppercase(), bytes)
            }
            _ => (detected.device_id, detected.mac_bytes),
        };

        let unique_id = server_identifier.replace('-', "").to_uppercase();

        let private_key = config
            .private_key
            .unwrap_or_else(|| derive_private_key(&server_identifier));

        Self {
            name,
            device_id,
            mac_bytes,
            server_identifier,
            unique_id,
            private_key,
        }
    }
}

/// Detect the user-facing computer name.
pub fn detect_computer_name() -> String {
    #[cfg(target_os = "macos")]
    {
        // 1. Try scutil --get ComputerName (e.g. "Yuhao's MacBook")
        for cmd in ["/usr/sbin/scutil", "scutil"] {
            if let Ok(output) = std::process::Command::new(cmd)
                .args(["--get", "ComputerName"])
                .output()
            {
                if output.status.success() {
                    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !name.is_empty() {
                        return name;
                    }
                }
            }
        }
        // 2. Try scutil --get LocalHostName
        for cmd in ["/usr/sbin/scutil", "scutil"] {
            if let Ok(output) = std::process::Command::new(cmd)
                .args(["--get", "LocalHostName"])
                .output()
            {
                if output.status.success() {
                    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !name.is_empty() {
                        return name;
                    }
                }
            }
        }
    }

    // Generic fallback: hostname command
    if let Ok(output) = std::process::Command::new("hostname").output() {
        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let clean = name.trim_end_matches(".local");
            if !clean.is_empty() {
                return clean.to_string();
            }
        }
    }

    DEFAULT_FALLBACK_NAME.to_string()
}

/// Detect the hardware UUID of the machine (e.g. IOPlatformUUID on macOS, machine-id on Linux).
pub fn detect_hardware_uuid() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        for cmd in ["/usr/sbin/ioreg", "ioreg"] {
            if let Ok(output) = std::process::Command::new(cmd)
                .args(["-rd1", "-c", "IOPlatformExpertDevice"])
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    for line in text.lines() {
                        if line.contains("IOPlatformUUID") {
                            if let Some(pos) = line.find('=') {
                                let val = line[pos + 1..]
                                    .trim()
                                    .trim_matches('"')
                                    .trim();
                                if let Ok(parsed) = Uuid::parse_str(val) {
                                    return Some(parsed.to_string().to_uppercase());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Ok(content) = fs::read_to_string(path) {
                let trimmed = content.trim();
                if let Ok(parsed) = Uuid::parse_str(trimmed) {
                    return Some(parsed.to_string().to_uppercase());
                }
            }
        }
    }

    None
}

/// Check if a MAC address is real and not all zeros or the macOS sandbox masked MAC (02:00:00:00:00:00).
pub fn is_valid_mac(bytes: &[u8; 6]) -> bool {
    bytes != &[0; 6] && bytes != &[0xFF; 6] && bytes != &[0x02, 0, 0, 0, 0, 0]
}

/// Detect the primary MAC address, or deterministically generate one from the server UUID.
pub fn detect_mac_address(server_uuid: &str) -> (String, [u8; 6]) {
    #[cfg(target_os = "macos")]
    {
        // 1. Try networksetup -listallhardwareports (most reliable, not masked by sandboxes)
        for cmd in ["/usr/sbin/networksetup", "networksetup"] {
            if let Ok(output) = std::process::Command::new(cmd)
                .arg("-listallhardwareports")
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8_lossy(&output.stdout);
                    // First pass: look specifically for Wi-Fi
                    let mut is_wifi = false;
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with("Hardware Port:") {
                            is_wifi = trimmed.contains("Wi-Fi");
                        } else if is_wifi && trimmed.starts_with("Ethernet Address:") {
                            if let Some(mac_str) = trimmed.strip_prefix("Ethernet Address:") {
                                if let Some(bytes) = parse_mac_address(mac_str.trim()) {
                                    if is_valid_mac(&bytes) {
                                        return (format_mac(&bytes), bytes);
                                    }
                                }
                            }
                        }
                    }
                    // Second pass: look for Ethernet
                    let mut is_ethernet = false;
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with("Hardware Port:") {
                            is_ethernet = trimmed.contains("Ethernet");
                        } else if is_ethernet && trimmed.starts_with("Ethernet Address:") {
                            if let Some(mac_str) = trimmed.strip_prefix("Ethernet Address:") {
                                if let Some(bytes) = parse_mac_address(mac_str.trim()) {
                                    if is_valid_mac(&bytes) {
                                        return (format_mac(&bytes), bytes);
                                    }
                                }
                            }
                        }
                    }
                    // Third pass: any valid hardware Ethernet Address
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with("Ethernet Address:") {
                            if let Some(mac_str) = trimmed.strip_prefix("Ethernet Address:") {
                                if let Some(bytes) = parse_mac_address(mac_str.trim()) {
                                    if is_valid_mac(&bytes) {
                                        return (format_mac(&bytes), bytes);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. Try ifconfig on interfaces en0..en4
        for iface in ["en0", "en1", "en2", "en3", "en4"] {
            for cmd in ["/sbin/ifconfig", "ifconfig"] {
                if let Ok(output) = std::process::Command::new(cmd).arg(iface).output() {
                    if output.status.success() {
                        let text = String::from_utf8_lossy(&output.stdout);
                        for line in text.lines() {
                            let trimmed = line.trim();
                            if trimmed.starts_with("ether ") {
                                if let Some(mac_str) = trimmed.strip_prefix("ether ") {
                                    let first_token = mac_str.split_whitespace().next().unwrap_or(mac_str);
                                    if let Some(bytes) = parse_mac_address(first_token) {
                                        if is_valid_mac(&bytes) {
                                            return (format_mac(&bytes), bytes);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(entries) = fs::read_dir("/sys/class/net") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str == "lo" {
                    continue;
                }
                let addr_file = entry.path().join("address");
                if let Ok(content) = fs::read_to_string(addr_file) {
                    let trimmed = content.trim();
                    if let Some(bytes) = parse_mac_address(trimmed) {
                        if bytes != [0; 6] {
                            return (format_mac(&bytes), bytes);
                        }
                    }
                }
            }
        }
    }

    // Deterministic fallback derived from server UUID
    let mut hasher = Sha256::new();
    hasher.update(b"atv-mac-seed-v1:");
    hasher.update(server_uuid.as_bytes());
    let hash = hasher.finalize();

    let mut bytes = [0u8; 6];
    bytes.copy_from_slice(&hash[..6]);
    // Set locally-administered unicast MAC bit: bit 1 of byte 0 is 1, bit 0 is 0
    bytes[0] = (bytes[0] | 0x02) & 0xFE;

    (format_mac(&bytes), bytes)
}

/// Derive a deterministic 32-byte persistent private key seed from the machine's UUID.
pub fn derive_private_key(server_uuid: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"atv-private-key-v1:");
    hasher.update(server_uuid.as_bytes());
    let digest = hasher.finalize();

    let mut key = [0u8; 32];
    key.copy_from_slice(&digest);
    key
}

/// Parse a MAC address string (with ':' or '-' delimiters, or 12 raw hex chars) into 6 bytes.
pub fn parse_mac_address(s: &str) -> Option<[u8; 6]> {
    let clean = s.replace([':', '-'], "");
    if clean.len() != 12 {
        return None;
    }
    let mut bytes = [0u8; 6];
    for i in 0..6 {
        bytes[i] = u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

/// Format 6 bytes as standard colon-delimited uppercase MAC address.
pub fn format_mac(bytes: &[u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
    )
}

/// Load a persistent UUID from `~/.config/atv/device_identity.json` or create and save a new one.
fn load_or_generate_persistent_uuid() -> String {
    let config_path = get_identity_file_path();
    if let Some(ref path) = config_path {
        if let Ok(content) = fs::read_to_string(path) {
            for line in content.lines() {
                if line.contains("server_identifier") {
                    if let Some(pos) = line.find(':') {
                        let val = line[pos + 1..]
                            .trim()
                            .trim_matches(|c| c == '"' || c == ',' || c == ' ')
                            .trim();
                        if let Ok(parsed) = Uuid::parse_str(val) {
                            return parsed.to_string().to_uppercase();
                        }
                    }
                }
            }
        }
    }

    let new_uuid = Uuid::new_v4().to_string().to_uppercase();
    if let Some(path) = config_path {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let json = format!("{{\n  \"server_identifier\": \"{new_uuid}\"\n}}\n");
        let _ = fs::write(path, json);
    }
    new_uuid
}

fn get_identity_file_path() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        return Some(PathBuf::from(home).join(".config/atv/device_identity.json"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mac_parsing_and_formatting() {
        let mac_str = "84:2f:57:2e:6c:ee";
        let bytes = parse_mac_address(mac_str).unwrap();
        assert_eq!(bytes, [0x84, 0x2f, 0x57, 0x2e, 0x6c, 0xee]);
        assert_eq!(format_mac(&bytes), "84:2F:57:2E:6C:EE");

        let raw_hex = "842F572E6CEE";
        let bytes2 = parse_mac_address(raw_hex).unwrap();
        assert_eq!(bytes2, bytes);

        let dashed = "84-2F-57-2E-6C-EE";
        let bytes3 = parse_mac_address(dashed).unwrap();
        assert_eq!(bytes3, bytes);

        assert!(parse_mac_address("invalid").is_none());
        assert!(parse_mac_address("12:34:56").is_none());
    }

    #[test]
    fn test_device_identity_deterministic() {
        let uuid = "F232C260-003A-5396-99D1-F8164BB03DF2";
        let key1 = derive_private_key(uuid);
        let key2 = derive_private_key(uuid);
        assert_eq!(key1, key2);

        let diff_uuid = "12345678-1234-1234-1234-123456789ABC";
        let key3 = derive_private_key(diff_uuid);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_detect_runs_without_panic() {
        let identity = DeviceIdentity::detect();
        println!("\n--- Detected Identity ---");
        println!("Name              : {}", identity.name);
        println!("Server Identifier : {}", identity.server_identifier);
        println!("Device ID (MAC)   : {}", identity.device_id);
        println!("Unique ID (Hex)   : {}", identity.unique_id);
        println!("-------------------------\n");
        assert!(!identity.name.is_empty());
        assert_eq!(identity.server_identifier.len(), 36);
        assert_eq!(identity.unique_id.len(), 32);
        assert_eq!(identity.device_id.len(), 17);
        assert_eq!(identity.private_key.len(), 32);
    }
}
