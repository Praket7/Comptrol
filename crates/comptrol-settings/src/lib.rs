//! Typed settings registry.
//!
//! Arbitrary registry/defaults/gsettings mutation is NOT a setting route.
//! Every setting is a declared descriptor with read, write, verify, and
//! rollback semantics plus platform/version metadata. Requests for unknown
//! keys refuse with `unknown_setting` instead of guessing a backing store.
//!
//! Protected settings (TCC-gated privacy controls, security policy) are
//! human-gated: the registry opens the exact settings surface and waits
//! for the user instead of writing through a back door.

pub mod providers;

pub use providers::provider_for;

use serde::{Deserialize, Serialize};

/// Closed set of settings Comptrol may manage. Adding a setting means
/// adding a descriptor with read/write/verify/rollback, not a string key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingKey {
    BluetoothEnabled,
    AudioOutputDevice,
    DisplayBrightness,
    NotificationsAppEnabled { app: String },
    DefaultBrowser,
    AccessibilityComptrolStatus,
    PrivacyMicrophoneAppStatus { app: String },
}

impl SettingKey {
    pub fn name(&self) -> String {
        match self {
            SettingKey::BluetoothEnabled => "settings.bluetooth.enabled".to_owned(),
            SettingKey::AudioOutputDevice => "settings.audio.output_device".to_owned(),
            SettingKey::DisplayBrightness => "settings.display.brightness".to_owned(),
            SettingKey::NotificationsAppEnabled { app } => {
                format!("settings.notifications.app_enabled:{app}")
            }
            SettingKey::DefaultBrowser => "settings.default_browser".to_owned(),
            SettingKey::AccessibilityComptrolStatus => {
                "settings.accessibility.comptrol_status".to_owned()
            }
            SettingKey::PrivacyMicrophoneAppStatus { app } => {
                format!("settings.privacy.microphone.app_status:{app}")
            }
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "settings.bluetooth.enabled" => Some(SettingKey::BluetoothEnabled),
            "settings.audio.output_device" => Some(SettingKey::AudioOutputDevice),
            "settings.display.brightness" => Some(SettingKey::DisplayBrightness),
            "settings.default_browser" => Some(SettingKey::DefaultBrowser),
            "settings.accessibility.comptrol_status" => {
                Some(SettingKey::AccessibilityComptrolStatus)
            }
            _ => {
                if let Some(app) = name.strip_prefix("settings.notifications.app_enabled:") {
                    Some(SettingKey::NotificationsAppEnabled {
                        app: app.to_owned(),
                    })
                } else {
                    name.strip_prefix("settings.privacy.microphone.app_status:")
                        .map(|app| SettingKey::PrivacyMicrophoneAppStatus {
                            app: app.to_owned(),
                        })
                }
            }
        }
    }

    /// Settings that no programmatic write may change; the user must act in
    /// the OS surface. `set` opens the surface and waits instead of writing.
    pub fn human_gated(&self) -> bool {
        matches!(
            self,
            SettingKey::AccessibilityComptrolStatus | SettingKey::PrivacyMicrophoneAppStatus { .. }
        )
    }

    /// Settings that can be rolled back after a non-security change.
    pub fn reversible(&self) -> bool {
        matches!(
            self,
            SettingKey::BluetoothEnabled
                | SettingKey::AudioOutputDevice
                | SettingKey::DisplayBrightness
                | SettingKey::NotificationsAppEnabled { .. }
                | SettingKey::DefaultBrowser
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum SettingValue {
    Bool { value: bool },
    Integer { value: i64 },
    Text { value: String },
}

impl SettingValue {
    pub fn bool(value: bool) -> Self {
        SettingValue::Bool { value }
    }

    pub fn equals(&self, other: &SettingValue) -> bool {
        self == other
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingObservation {
    pub key: SettingKey,
    pub value: SettingValue,
    pub route: String,
    pub readback_verified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("unknown setting: {0}")]
    UnknownSetting(String),
    #[error("setting {key} is not supported on this platform: {reason}")]
    UnsupportedPlatform { key: String, reason: String },
    #[error("setting {key} requires the user to act in {surface}; opened for approval")]
    HumanActionRequired { key: String, surface: String },
    #[error("write refused for {key}: {reason}")]
    WriteRefused { key: String, reason: String },
    #[error("verification failed for {key}: expected {expected:?}, observed {observed:?}")]
    VerificationFailed {
        key: String,
        expected: String,
        observed: String,
    },
    #[error("provider failed for {key}: {message}")]
    ProviderFailed { key: String, message: String },
}

/// Read a setting through its declared read surface.
pub fn get(key: &SettingKey) -> Result<SettingObservation, SettingsError> {
    let provider = provider_for()?;
    provider.read(key)
}

/// Write a setting, then verify with an independent readback. Human-gated
/// settings open an exact OS surface only when the provider declares one;
/// otherwise the request refuses rather than inventing a generic approval route.
pub fn set(key: &SettingKey, value: SettingValue) -> Result<SettingObservation, SettingsError> {
    if key.human_gated() {
        let provider = provider_for()?;
        let capabilities = provider.capabilities(key);
        if !capabilities.human_surface_available {
            return Err(SettingsError::UnsupportedPlatform {
                key: key.name(),
                reason: "no exact documented human settings surface is wired for this protected setting on this platform".to_owned(),
            });
        }
        let surface = provider.human_surface(key);
        if surface.is_empty() {
            return Err(SettingsError::UnsupportedPlatform {
                key: key.name(),
                reason: "the provider declared no exact human settings surface for this protected setting".to_owned(),
            });
        }
        return Err(SettingsError::HumanActionRequired {
            key: key.name(),
            surface,
        });
    }
    let provider = provider_for()?;
    provider.write(key, &value)?;
    let observed = provider.read(key)?;
    if !observed.value.equals(&value) {
        return Err(SettingsError::VerificationFailed {
            key: key.name(),
            expected: format!("{value:?}"),
            observed: format!("{:?}", observed.value),
        });
    }
    Ok(SettingObservation {
        key: key.clone(),
        value,
        route: observed.route,
        readback_verified: true,
    })
}

/// Roll back a reversible setting to a previous observed value.
pub fn rollback(
    key: &SettingKey,
    previous: SettingValue,
) -> Result<SettingObservation, SettingsError> {
    if !key.reversible() {
        return Err(SettingsError::WriteRefused {
            key: key.name(),
            reason: "this setting is not reversible through Comptrol".to_owned(),
        });
    }
    set(key, previous)
}

/// All declared settings with platform support metadata.
pub fn registry() -> Vec<serde_json::Value> {
    SettingKey::all()
        .into_iter()
        .map(|key| {
            let provider = provider_for();
            let caps = provider
                .as_ref()
                .map(|p| p.capabilities(&key))
                .unwrap_or(crate::providers::SettingCapabilities {
                    readable: false,
                    writable: false,
                    human_surface_available: false,
                    readback_verifiable: false,
                });
            serde_json::json!({
                "key": key.name(),
                "human_gated": key.human_gated(),
                "reversible": key.reversible(),
                "supported_on_this_platform": caps.readable || caps.writable || caps.human_surface_available,
                "readable": caps.readable,
                "writable": caps.writable,
                "human_surface_available": caps.human_surface_available,
                "readback_verifiable": caps.readback_verifiable,
            })
        })
        .collect()
}

impl SettingKey {
    pub fn all() -> Vec<SettingKey> {
        vec![
            SettingKey::BluetoothEnabled,
            SettingKey::AudioOutputDevice,
            SettingKey::DisplayBrightness,
            SettingKey::NotificationsAppEnabled {
                app: "example".to_owned(),
            },
            SettingKey::DefaultBrowser,
            SettingKey::AccessibilityComptrolStatus,
            SettingKey::PrivacyMicrophoneAppStatus {
                app: "example".to_owned(),
            },
        ]
    }
}

#[cfg(test)]
mod tests;
