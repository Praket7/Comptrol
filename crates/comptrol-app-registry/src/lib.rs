//! Installed-application registry and native launch.
//!
//! Replaces weak app-name launching: the registry resolves the exact
//! installed application through the platform's own registration
//! mechanisms (LaunchServices on macOS, Start Menu / shell registration
//! on Windows, desktop entries on Linux), launches it through the native
//! OS mechanism, and verifies the resulting process identity.
//!
//! Launch verification follows the V5 rule: a successfully started
//! process is *delivery*, not *verification*. Verification requires the
//! process identity (pid plus platform-specific identity) to be observed
//! alive after launch, and, when a resource was opened, requires the
//! resource identity to round-trip.

pub mod launch;
pub mod registry;

pub use launch::{
    LaunchOutcome, LaunchRequest, LaunchVerification, launch, launch_verified, launcher_probe,
};
pub use registry::{AppEntry, RegistryError, resolve, resolve_all};

/// What the caller asked to open.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Resource {
    File {
        path: String,
    },
    Url {
        url: String,
    },
    DeepLink {
        uri: String,
    },
    #[default]
    None,
}

#[cfg(test)]
mod tests;
