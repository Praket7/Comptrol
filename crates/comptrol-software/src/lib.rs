//! First-class software discovery, installation, update, and uninstall.
//!
//! V5 rule: a package manager's successful exit code is *delivery*, not
//! *verification*. Verification requires the installed package inventory to
//! contain the exact package id and version afterwards, plus the resulting
//! application registry entry where the platform exposes one.
//!
//! Safety invariants enforced here, not left to callers:
//!
//! - Only exact package identities install. Fuzzy search results are
//!   returned for the user to choose from; they never install directly.
//! - License/agreement flags are never added unless the caller passes
//!   `accept_agreements: true` for the exact package and version shown.
//! - Elevation is never performed here. When the platform reports that
//!   privilege elevation is required, installation stops with
//!   [`SoftwareError::ElevationRequired`] so the runtime can raise a
//!   `human_action_required` challenge and let the user approve in the
//!   native prompt. Passwords are never collected, piped, or stored.
//! - All provider invocations use argv with no shell.

pub mod providers;

pub use providers::{Provider, ProviderId, available_providers, provider_for};

use serde::{Deserialize, Serialize};

/// Exact package identity. The `id` is provider-scoped
/// (`winget:VideoLAN.VLC`, `brew:vlc`, `flatpak:org.videolan.VLC`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageId {
    pub provider: ProviderId,
    pub id: String,
}

impl PackageId {
    pub fn new(provider: ProviderId, id: impl Into<String>) -> Self {
        Self {
            provider,
            id: id.into(),
        }
    }
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.provider.as_str(), self.id)
    }
}

/// Publisher/source/version metadata surfaced *before* install.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PackageDescription {
    pub package: String,
    pub name: String,
    pub publisher: Option<String>,
    pub source: Option<String>,
    pub version: Option<String>,
    pub installer_type: Option<String>,
    pub agreements: Vec<String>,
    pub installed_version: Option<String>,
}

/// What the caller asked to change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstallRequest {
    pub package: String,
    pub provider: Option<ProviderId>,
    pub version: Option<String>,
    pub source: Option<String>,
    /// Must be true for the exact package/version shown, otherwise the
    /// provider refuses with [`SoftwareError::AgreementsRequired`].
    pub accept_agreements: bool,
    pub launch_after_install: bool,
}

impl InstallRequest {
    pub fn new(package: impl Into<String>) -> Self {
        Self {
            package: package.into(),
            provider: None,
            version: None,
            source: None,
            accept_agreements: false,
            launch_after_install: false,
        }
    }
}

/// Verification evidence for an install/update/uninstall outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum InstallVerification {
    /// Inventory re-query shows the exact id and version.
    Verified { package: String, version: String },
    /// Inventory does not yet show the package.
    Unverified { package: String, reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftwareOutcome {
    pub package: String,
    pub provider: ProviderId,
    pub version: Option<String>,
    pub route: String,
    pub verification: InstallVerification,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SoftwareError {
    #[error("no software provider is available on this platform: {0}")]
    NoProvider(String),
    #[error("package not found: {0}")]
    NotFound(String),
    #[error("ambiguous package reference {reference}: {candidates:?}")]
    Ambiguous {
        reference: String,
        candidates: Vec<String>,
    },
    #[error("package {package} requires the user to accept agreements first: {agreements:?}")]
    AgreementsRequired {
        package: String,
        agreements: Vec<String>,
    },
    #[error("elevation required for {package} via {provider}; user approval needed")]
    ElevationRequired { package: String, provider: String },
    #[error("provider {provider} failed: {message}")]
    ProviderFailed { provider: String, message: String },
    #[error("verification failed for {package}: {reason}")]
    VerificationFailed { package: String, reason: String },
    #[error("unsupported operation: {0}")]
    Unsupported(String),
}

/// Search installed and remote packages. Never installs.
pub fn search(
    query: &str,
    provider: Option<ProviderId>,
) -> Result<Vec<PackageDescription>, SoftwareError> {
    let provider = provider_for(provider)?;
    provider.search(query)
}

/// Describe one exact package, including agreements and installed state.
pub fn describe(
    package: &str,
    provider: Option<ProviderId>,
) -> Result<PackageDescription, SoftwareError> {
    let provider = provider_for(provider)?;
    provider.describe_exact(package)
}

/// Install one exact package after agreement gating and inventory verification.
pub fn install(request: &InstallRequest) -> Result<SoftwareOutcome, SoftwareError> {
    let provider = provider_for(request.provider)?;
    // Exactness gate: the reference must resolve to exactly one package.
    let candidates = provider.search(&request.package)?;
    let exact: Vec<&PackageDescription> = candidates
        .iter()
        .filter(|candidate| {
            candidate.package.eq_ignore_ascii_case(&request.package)
                || candidate.name.eq_ignore_ascii_case(&request.package)
        })
        .collect();
    if exact.is_empty() {
        return Err(SoftwareError::NotFound(request.package.clone()));
    }
    if exact.len() > 1 {
        return Err(SoftwareError::Ambiguous {
            reference: request.package.clone(),
            candidates: exact.iter().map(|c| c.package.clone()).collect(),
        });
    }
    let described = provider.describe_exact(&exact[0].package)?;
    if !described.agreements.is_empty() && !request.accept_agreements {
        return Err(SoftwareError::AgreementsRequired {
            package: described.package.clone(),
            agreements: described.agreements.clone(),
        });
    }
    if let Some(want) = &request.version
        && let Some(have) = &described.installed_version
        && have == want
    {
        return Ok(SoftwareOutcome {
            package: described.package.clone(),
            provider: provider.id(),
            version: Some(have.clone()),
            route: provider.route_name("install"),
            verification: InstallVerification::Verified {
                package: described.package.clone(),
                version: have.clone(),
            },
        });
    }
    provider.install(request, &described)?;
    // Independent verification: re-query the inventory.
    let after = provider.describe_exact(&described.package)?;
    match after.installed_version {
        Some(version) => {
            if let Some(want) = &request.version
                && &version != want
            {
                return Err(SoftwareError::VerificationFailed {
                    package: after.package.clone(),
                    reason: format!("installed version {version} does not match requested {want}"),
                });
            }
            Ok(SoftwareOutcome {
                package: after.package.clone(),
                provider: provider.id(),
                version: Some(version.clone()),
                route: provider.route_name("install"),
                verification: InstallVerification::Verified {
                    package: after.package.clone(),
                    version,
                },
            })
        }
        None => Err(SoftwareError::VerificationFailed {
            package: after.package.clone(),
            reason: "package inventory does not list the package after install".to_owned(),
        }),
    }
}

/// Update one exact installed package.
pub fn update(
    package: &str,
    provider: Option<ProviderId>,
) -> Result<SoftwareOutcome, SoftwareError> {
    let provider = provider_for(provider)?;
    let before = provider.describe_exact(package)?;
    if before.installed_version.is_none() {
        return Err(SoftwareError::NotFound(package.to_owned()));
    }
    provider.update(package)?;
    let after = provider.describe_exact(package)?;
    match after.installed_version {
        Some(version) => Ok(SoftwareOutcome {
            package: after.package.clone(),
            provider: provider.id(),
            version: Some(version.clone()),
            route: provider.route_name("update"),
            verification: InstallVerification::Verified {
                package: after.package.clone(),
                version,
            },
        }),
        None => Err(SoftwareError::VerificationFailed {
            package: after.package.clone(),
            reason: "package missing from inventory after update".to_owned(),
        }),
    }
}

/// Uninstall one exact installed package; verifies absence afterwards.
pub fn uninstall(
    package: &str,
    provider: Option<ProviderId>,
) -> Result<SoftwareOutcome, SoftwareError> {
    let provider = provider_for(provider)?;
    let described = provider.describe_exact(package)?;
    if described.installed_version.is_none() {
        return Err(SoftwareError::NotFound(package.to_owned()));
    }
    provider.uninstall(package)?;
    let after = provider.describe_exact(package)?;
    if after.installed_version.is_some() {
        return Err(SoftwareError::VerificationFailed {
            package: after.package.clone(),
            reason: "package still present in inventory after uninstall".to_owned(),
        });
    }
    Ok(SoftwareOutcome {
        package: after.package.clone(),
        provider: provider.id(),
        version: None,
        route: provider.route_name("uninstall"),
        verification: InstallVerification::Verified {
            package: after.package.clone(),
            version: "removed".to_owned(),
        },
    })
}

#[cfg(test)]
mod tests;
