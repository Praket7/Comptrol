//! Platform setting providers.
//!
//! Providers use documented surfaces only: `ms-settings:` URIs and public
//! configuration APIs on Windows, XDG desktop contracts on Linux, and
//! supported preference surfaces on macOS. There is intentionally no generic "write this
//! registry key / defaults domain" escape hatch.

use crate::{SettingKey, SettingObservation, SettingValue, SettingsError};
use std::process::Command;

/// Declarative capability metadata for a setting on a given platform.
/// No read or write is performed during capability enumeration.
#[derive(Clone, Debug)]
pub struct SettingCapabilities {
    pub readable: bool,
    pub writable: bool,
    pub human_surface_available: bool,
    pub readback_verifiable: bool,
}

pub trait SettingProvider {
    fn supports(&self, key: &SettingKey) -> bool;
    fn capabilities(&self, key: &SettingKey) -> SettingCapabilities;
    fn human_surface(&self, key: &SettingKey) -> String;
    fn read(&self, key: &SettingKey) -> Result<SettingObservation, SettingsError>;
    fn write(&self, key: &SettingKey, value: &SettingValue) -> Result<(), SettingsError>;
}

fn run(argv0: &str, args: &[&str]) -> Result<String, SettingsError> {
    let output =
        Command::new(argv0)
            .args(args)
            .output()
            .map_err(|e| SettingsError::ProviderFailed {
                key: String::new(),
                message: e.to_string(),
            })?;
    if !output.status.success() {
        return Err(SettingsError::ProviderFailed {
            key: String::new(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub struct WindowsProvider;

impl SettingProvider for WindowsProvider {
    fn supports(&self, key: &SettingKey) -> bool {
        // Windows has no programmatic read or write surfaces wired in this
        // build. All operations return UnsupportedPlatform/WriteRefused.
        // Only human_surface() is available for opening the Settings page.
        let _ = key;
        false
    }

    fn capabilities(&self, key: &SettingKey) -> SettingCapabilities {
        let _ = key;
        SettingCapabilities {
            readable: false,
            writable: false,
            human_surface_available: true,
            readback_verifiable: false,
        }
    }

    fn human_surface(&self, key: &SettingKey) -> String {
        match key {
            SettingKey::AccessibilityComptrolStatus => {
                "ms-settings:privacy-accessibility".to_owned()
            }
            SettingKey::BluetoothEnabled => "ms-settings:bluetooth".to_owned(),
            _ => "ms-settings:apps-defaults".to_owned(),
        }
    }

    fn read(&self, key: &SettingKey) -> Result<SettingObservation, SettingsError> {
        Err(SettingsError::UnsupportedPlatform {
            key: key.name(),
            reason: "Windows programmatic read for this setting is not wired in this build; use the ms-settings: surface".to_owned(),
        })
    }

    fn write(&self, key: &SettingKey, _value: &SettingValue) -> Result<(), SettingsError> {
        // Security-sensitive registry values are never written directly even
        // when they exist. UI-gated settings go through human_surface.
        Err(SettingsError::WriteRefused {
            key: key.name(),
            reason: "no official programmatic write surface is wired for this setting; open the exact Settings page for the user".to_owned(),
        })
    }
}

pub struct MacosProvider;

impl SettingProvider for MacosProvider {
    fn supports(&self, key: &SettingKey) -> bool {
        // Do not parse the private LSHandlers preferences database as if it
        // were an authoritative default-browser API. A safe Launch Services
        // framework binding is not wired in this crate yet.
        let _ = key;
        false
    }

    fn capabilities(&self, key: &SettingKey) -> SettingCapabilities {
        let human_surface_available = matches!(
            key,
            SettingKey::DefaultBrowser
                | SettingKey::BluetoothEnabled
                | SettingKey::AccessibilityComptrolStatus
                | SettingKey::PrivacyMicrophoneAppStatus { .. }
        );
        SettingCapabilities {
            readable: false,
            writable: false,
            human_surface_available,
            readback_verifiable: false,
        }
    }

    fn human_surface(&self, key: &SettingKey) -> String {
        match key {
            SettingKey::AccessibilityComptrolStatus => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
                    .to_owned()
            }
            SettingKey::PrivacyMicrophoneAppStatus { .. } => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
                    .to_owned()
            }
            SettingKey::BluetoothEnabled => {
                "x-apple.systempreferences:com.apple.preference.bluetooth".to_owned()
            }
            SettingKey::DefaultBrowser => "x-apple.systempreferences:".to_owned(),
            _ => "x-apple.systempreferences:".to_owned(),
        }
    }

    fn read(&self, key: &SettingKey) -> Result<SettingObservation, SettingsError> {
        Err(SettingsError::UnsupportedPlatform {
            key: key.name(),
            reason: match key {
                SettingKey::DefaultBrowser => "the authoritative Launch Services default-handler API is not safely bound in this build; private LSHandlers preferences are intentionally not parsed".to_owned(),
                _ => "macOS read for this setting needs a supported framework call not wired in this build".to_owned(),
            },
        })
    }

    fn write(&self, key: &SettingKey, _value: &SettingValue) -> Result<(), SettingsError> {
        // The TCC database and private LaunchServices preference files are never written.
        Err(SettingsError::WriteRefused {
            key: key.name(),
            reason: "macOS writes go through supported prompts and System Settings with the user; direct private-preference writes are refused".to_owned(),
        })
    }
}

pub struct LinuxProvider;

impl LinuxProvider {
    #[cfg(target_os = "linux")]
    fn detect() -> Self {
        Self
    }
}

impl SettingProvider for LinuxProvider {
    fn supports(&self, key: &SettingKey) -> bool {
        // xdg-settings is the desktop contract for the default browser.
        // DisplayBrightness is intentionally not mapped to GNOME
        // idle-brightness: that key controls dimming policy, not panel brightness.
        matches!(key, SettingKey::DefaultBrowser)
    }

    fn capabilities(&self, key: &SettingKey) -> SettingCapabilities {
        match key {
            SettingKey::DefaultBrowser => SettingCapabilities {
                readable: true,
                writable: false,
                human_surface_available: true,
                readback_verifiable: true,
            },
            SettingKey::DisplayBrightness => SettingCapabilities {
                readable: false,
                writable: false,
                human_surface_available: true,
                readback_verifiable: false,
            },
            _ => SettingCapabilities {
                readable: false,
                writable: false,
                human_surface_available: true,
                readback_verifiable: false,
            },
        }
    }

    fn human_surface(&self, key: &SettingKey) -> String {
        match key {
            SettingKey::BluetoothEnabled => "gnome-control-center bluetooth".to_owned(),
            SettingKey::DisplayBrightness => "gnome-control-center display".to_owned(),
            SettingKey::DefaultBrowser => "xdg-settings get default-web-browser".to_owned(),
            _ => "gnome-control-center".to_owned(),
        }
    }

    fn read(&self, key: &SettingKey) -> Result<SettingObservation, SettingsError> {
        match key {
            SettingKey::DefaultBrowser => {
                let value = run("xdg-settings", &["get", "default-web-browser"]).map_err(|error| {
                    SettingsError::UnsupportedPlatform {
                        key: key.name(),
                        reason: format!("xdg-settings default browser lookup is unavailable: {error}"),
                    }
                })?;
                if value.is_empty() {
                    return Err(SettingsError::UnsupportedPlatform {
                        key: key.name(),
                        reason: "xdg-settings returned no default web browser".to_owned(),
                    });
                }
                Ok(SettingObservation {
                    key: key.clone(),
                    value: SettingValue::Text { value },
                    route: "linux_xdg_settings_read".to_owned(),
                    readback_verified: true,
                })
            }
            SettingKey::DisplayBrightness => Err(SettingsError::UnsupportedPlatform {
                key: key.name(),
                reason: "panel brightness requires an exact backlight/display identity; GNOME idle-brightness is a dimming-policy value and is intentionally not used".to_owned(),
            }),
            _ => Err(SettingsError::UnsupportedPlatform {
                key: key.name(),
                reason: "no stable documented read surface is wired for this setting on this desktop".to_owned(),
            }),
        }
    }

    fn write(&self, key: &SettingKey, _value: &SettingValue) -> Result<(), SettingsError> {
        Err(SettingsError::WriteRefused {
            key: key.name(),
            reason: match key {
                SettingKey::DisplayBrightness => "no exact panel/backlight identity is present in this setting request, so writing a sysfs backlight or dimming-policy key would be ambiguous".to_owned(),
                _ => "no stable documented write surface is wired for this setting on this desktop; use the Settings UI route".to_owned(),
            },
        })
    }
}

/// Platform provider for this build.
pub fn provider_for() -> Result<Box<dyn SettingProvider>, SettingsError> {
    #[cfg(target_os = "windows")]
    return Ok(Box::new(WindowsProvider));
    #[cfg(target_os = "macos")]
    return Ok(Box::new(MacosProvider));
    #[cfg(target_os = "linux")]
    return Ok(Box::new(LinuxProvider::detect()));
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    return Err(SettingsError::UnsupportedPlatform {
        key: String::new(),
        reason: "unsupported platform".to_owned(),
    });
}
