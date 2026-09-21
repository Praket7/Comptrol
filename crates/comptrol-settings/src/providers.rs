//! Platform setting providers.
//!
//! Providers use documented surfaces only: `ms-settings:` URIs and public
//! configuration APIs on Windows, GSettings on GNOME, supported preference
//! surfaces on macOS. There is intentionally no generic "write this
//! registry key / defaults domain" escape hatch.

use crate::{SettingKey, SettingObservation, SettingValue, SettingsError};
use std::process::Command;

pub trait SettingProvider {
    fn supports(&self, key: &SettingKey) -> bool;
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
        // Only DefaultBrowser can be read programmatically via LaunchServices
        // defaults read. All other settings have no wired read/write surface.
        matches!(key, SettingKey::DefaultBrowser)
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
            _ => "x-apple.systempreferences:".to_owned(),
        }
    }

    fn read(&self, key: &SettingKey) -> Result<SettingObservation, SettingsError> {
        match key {
            SettingKey::DefaultBrowser => {
                // LaunchServices default http handler via `defaults` read of
                // the documented plist domain (read-only).
                match run("defaults", &["read", "com.apple.LaunchServices/com.apple.launchservices.secure", "LSHandlers"]) {
                    Ok(text) => Ok(SettingObservation {
                        key: key.clone(),
                        value: SettingValue::Text {
                            value: text.lines().next().unwrap_or("").to_owned(),
                        },
                        route: "macos_launchservices_read".to_owned(),
                        readback_verified: false,
                    }),
                    Err(_) => Err(SettingsError::UnsupportedPlatform {
                        key: key.name(),
                        reason: "default browser could not be read on this macOS version".to_owned(),
                    }),
                }
            }
            _ => Err(SettingsError::UnsupportedPlatform {
                key: key.name(),
                reason: "macOS read for this setting needs a supported framework call not wired in this build".to_owned(),
            }),
        }
    }

    fn write(&self, key: &SettingKey, _value: &SettingValue) -> Result<(), SettingsError> {
        // The TCC database is never written. Ever.
        Err(SettingsError::WriteRefused {
            key: key.name(),
            reason: "macOS writes go through supported prompts and System Settings with the user; direct writes are refused".to_owned(),
        })
    }
}

pub struct LinuxProvider {
    desktop: String,
}

impl LinuxProvider {
    #[cfg(target_os = "linux")]
    fn detect() -> Self {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .or_else(|_| std::env::var("DESKTOP_SESSION"))
            .unwrap_or_default()
            .to_lowercase();
        Self { desktop }
    }

    fn gsettings_get(&self, schema: &str, key: &str) -> Result<String, SettingsError> {
        run("gsettings", &["get", schema, key])
    }

    fn gsettings_set(&self, schema: &str, key: &str, value: &str) -> Result<(), SettingsError> {
        run("gsettings", &["set", schema, key, value]).map(|_| ())
    }
}

impl SettingProvider for LinuxProvider {
    fn supports(&self, key: &SettingKey) -> bool {
        if !(self.desktop.contains("gnome")
            || self.desktop.contains("unity")
            || self.desktop.is_empty())
        {
            return false;
        }
        // DisplayBrightness is readable and writable via GSettings.
        // DefaultBrowser is readable via GSettings (read-only).
        // All other settings have no wired surface on Linux.
        matches!(
            key,
            SettingKey::DisplayBrightness | SettingKey::DefaultBrowser
        )
    }

    fn human_surface(&self, key: &SettingKey) -> String {
        match key {
            SettingKey::BluetoothEnabled => "gnome-control-center bluetooth".to_owned(),
            _ => "gnome-control-center".to_owned(),
        }
    }

    fn read(&self, key: &SettingKey) -> Result<SettingObservation, SettingsError> {
        let unsupported = |reason: &str| SettingsError::UnsupportedPlatform {
            key: key.name(),
            reason: reason.to_owned(),
        };
        match key {
            SettingKey::DisplayBrightness => {
                let text = self
                    .gsettings_get("org.gnome.settings-daemon.plugins.power", "idle-brightness")
                    .map_err(|_| {
                        unsupported("GNOME GSettings brightness is unavailable in this session")
                    })?;
                Ok(SettingObservation {
                    key: key.clone(),
                    value: SettingValue::Integer {
                        value: text.parse().unwrap_or(0),
                    },
                    route: "gnome_gsettings_read".to_owned(),
                    readback_verified: false,
                })
            }
            SettingKey::DefaultBrowser => {
                let text = self
                    .gsettings_get("org.gnome.desktop.default-applications", "browser")
                    .or_else(|_| {
                        // Fallback: try the older schema path
                        self.gsettings_get(
                            "org.gnome.desktop.default-applications.internet",
                            "browser",
                        )
                    })
                    .map_err(|_| unsupported("GNOME default browser lookup failed"))?;
                Ok(SettingObservation {
                    key: key.clone(),
                    value: SettingValue::Text { value: text },
                    route: "gnome_gsettings_read".to_owned(),
                    readback_verified: false,
                })
            }
            _ => Err(unsupported(
                "no stable documented read surface is wired for this setting on this desktop",
            )),
        }
    }

    fn write(&self, key: &SettingKey, value: &SettingValue) -> Result<(), SettingsError> {
        match (key, value) {
            (SettingKey::DisplayBrightness, SettingValue::Integer { value }) => {
                self.gsettings_set(
                    "org.gnome.settings-daemon.plugins.power",
                    "idle-brightness",
                    &value.to_string(),
                )
            }
            _ => Err(SettingsError::WriteRefused {
                key: key.name(),
                reason: "no stable documented write surface is wired for this setting on this desktop; use the Settings UI route".to_owned(),
            }),
        }
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
