//! Platform software providers.
//!
//! Each provider shells out to the platform's trusted mechanism with argv
//! only (never a shell string) and parses machine-readable output. A
//! provider that is not installed reports itself unavailable; the runtime
//! surfaces that through capabilities instead of pretending support.

use crate::{InstallRequest, PackageDescription, SoftwareError};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    WinGet,
    Homebrew,
    PackageKit,
    Flatpak,
    Apt,
    Dnf,
    MacAppStore,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::WinGet => "winget",
            ProviderId::Homebrew => "brew",
            ProviderId::PackageKit => "packagekit",
            ProviderId::Flatpak => "flatpak",
            ProviderId::Apt => "apt",
            ProviderId::Dnf => "dnf",
            ProviderId::MacAppStore => "appstore",
        }
    }
}

pub trait Provider {
    fn id(&self) -> ProviderId;
    fn route_name(&self, op: &str) -> String {
        format!("{}:{op}", self.id().as_str())
    }
    fn available(&self) -> bool;
    fn search(&self, query: &str) -> Result<Vec<PackageDescription>, SoftwareError>;
    fn describe_exact(&self, package: &str) -> Result<PackageDescription, SoftwareError>;
    fn install(
        &self,
        request: &InstallRequest,
        described: &PackageDescription,
    ) -> Result<(), SoftwareError>;
    fn update(&self, package: &str) -> Result<(), SoftwareError>;
    fn uninstall(&self, package: &str) -> Result<(), SoftwareError>;
}

fn find_binary(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for suffix in [name.to_owned(), format!("{name}.exe")] {
            let candidate = dir.join(&suffix);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn elevation_hint(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("elevation")
        || lower.contains("uac")
        || lower.contains("access is denied")
        || lower.contains("requires administration")
        || lower.contains("authentication is required")
        || lower.contains("polkit")
        || lower.contains("not authorized")
}

/// Windows Package Manager. Prefers machine-readable output; surfaces UAC
/// as [`SoftwareError::ElevationRequired`] for human-action handoff.
pub struct WinGetProvider {
    binary: Option<PathBuf>,
}

impl WinGetProvider {
    pub fn new() -> Self {
        Self {
            binary: find_binary("winget"),
        }
    }
}

impl Default for WinGetProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for WinGetProvider {
    fn id(&self) -> ProviderId {
        ProviderId::WinGet
    }

    fn available(&self) -> bool {
        cfg!(target_os = "windows") && self.binary.is_some()
    }

    fn search(&self, query: &str) -> Result<Vec<PackageDescription>, SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "winget is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["search", query, "--accept-source-agreements"])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "winget".to_owned(),
                message: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(SoftwareError::ProviderFailed {
                provider: "winget".to_owned(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        // Table output is locale-dependent; parse conservatively into
        // candidate rows and let describe_exact establish exact identity.
        let text = String::from_utf8_lossy(&output.stdout);
        let mut results = Vec::new();
        for line in text.lines().skip(2) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('-') {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                results.push(PackageDescription {
                    package: parts[parts.len() - 2].to_owned(),
                    name: parts[..parts.len() - 1].join(" "),
                    source: Some("winget".to_owned()),
                    version: parts.last().map(|s| (*s).to_owned()),
                    ..Default::default()
                });
            }
        }
        Ok(results)
    }

    fn describe_exact(&self, package: &str) -> Result<PackageDescription, SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "winget is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["show", "--id", package, "--accept-source-agreements"])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "winget".to_owned(),
                message: e.to_string(),
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No package found") || stderr.contains("not found") {
                return Err(SoftwareError::NotFound(package.to_owned()));
            }
            return Err(SoftwareError::ProviderFailed {
                provider: "winget".to_owned(),
                message: stderr.trim().to_owned(),
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut described = PackageDescription {
            package: package.to_owned(),
            name: package.to_owned(),
            source: Some("winget".to_owned()),
            ..Default::default()
        };
        for line in text.lines() {
            if let Some((key, value)) = line.split_once(':') {
                match key.trim().to_lowercase().as_str() {
                    "publisher" => described.publisher = Some(value.trim().to_owned()),
                    "version" => described.version = Some(value.trim().to_owned()),
                    "installer type" => described.installer_type = Some(value.trim().to_owned()),
                    _ => {}
                }
            }
        }
        // Installed state via `winget list --id`.
        if let Ok(list) = Command::new(binary)
            .args(["list", "--id", package])
            .output()
            && list.status.success()
        {
            let list_text = String::from_utf8_lossy(&list.stdout);
            for line in list_text.lines().skip(2) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[parts.len() - 2].eq_ignore_ascii_case(package) {
                    described.installed_version = parts.last().map(|s| (*s).to_owned());
                }
            }
        }
        Ok(described)
    }

    fn install(
        &self,
        request: &InstallRequest,
        described: &PackageDescription,
    ) -> Result<(), SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "winget is not installed".to_owned(),
            ));
        };
        let mut args = vec![
            "install".to_owned(),
            "--id".to_owned(),
            described.package.clone(),
            "--exact".to_owned(),
            "--accept-source-agreements".to_owned(),
            "--disable-interactivity".to_owned(),
            "--silent".to_owned(),
        ];
        if let Some(version) = &request.version {
            args.push("--version".to_owned());
            args.push(version.clone());
        }
        if request.accept_agreements {
            args.push("--accept-package-agreements".to_owned());
        }
        let output = Command::new(binary).args(&args).output().map_err(|e| {
            SoftwareError::ProviderFailed {
                provider: "winget".to_owned(),
                message: e.to_string(),
            }
        })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if elevation_hint(&stderr) {
            return Err(SoftwareError::ElevationRequired {
                package: described.package.clone(),
                provider: "winget".to_owned(),
            });
        }
        Err(SoftwareError::ProviderFailed {
            provider: "winget".to_owned(),
            message: stderr.trim().to_owned(),
        })
    }

    fn update(&self, package: &str) -> Result<(), SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "winget is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args([
                "upgrade",
                "--id",
                package,
                "--exact",
                "--accept-source-agreements",
                "--silent",
            ])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "winget".to_owned(),
                message: e.to_string(),
            })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if elevation_hint(&stderr) {
            return Err(SoftwareError::ElevationRequired {
                package: package.to_owned(),
                provider: "winget".to_owned(),
            });
        }
        Err(SoftwareError::ProviderFailed {
            provider: "winget".to_owned(),
            message: stderr.trim().to_owned(),
        })
    }

    fn uninstall(&self, package: &str) -> Result<(), SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "winget is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["uninstall", "--id", package, "--exact", "--silent"])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "winget".to_owned(),
                message: e.to_string(),
            })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if elevation_hint(&stderr) {
            return Err(SoftwareError::ElevationRequired {
                package: package.to_owned(),
                provider: "winget".to_owned(),
            });
        }
        Err(SoftwareError::ProviderFailed {
            provider: "winget".to_owned(),
            message: stderr.trim().to_owned(),
        })
    }
}

/// Homebrew formula/cask provider for macOS (and Linuxbrew).
pub struct HomebrewProvider {
    binary: Option<PathBuf>,
}

impl HomebrewProvider {
    pub fn new() -> Self {
        Self {
            binary: find_binary("brew"),
        }
    }
}

impl Default for HomebrewProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for HomebrewProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Homebrew
    }

    fn available(&self) -> bool {
        self.binary.is_some()
    }

    fn search(&self, query: &str) -> Result<Vec<PackageDescription>, SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "brew is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["search", query])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "brew".to_owned(),
                message: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(SoftwareError::ProviderFailed {
                provider: "brew".to_owned(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("==>"))
            .map(|line| PackageDescription {
                package: line.to_owned(),
                name: line.to_owned(),
                source: Some("homebrew".to_owned()),
                ..Default::default()
            })
            .collect())
    }

    fn describe_exact(&self, package: &str) -> Result<PackageDescription, SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "brew is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["info", "--json=v2", package])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "brew".to_owned(),
                message: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(SoftwareError::NotFound(package.to_owned()));
        }
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|_| SoftwareError::ProviderFailed {
                provider: "brew".to_owned(),
                message: "brew info returned unparseable JSON".to_owned(),
            })?;
        let mut described = PackageDescription {
            package: package.to_owned(),
            name: package.to_owned(),
            source: Some("homebrew".to_owned()),
            ..Default::default()
        };
        for list in ["formulae", "casks"].iter() {
            if let Some(entries) = json.get(*list).and_then(|v| v.as_array()) {
                for entry in entries {
                    let token = entry
                        .get("token")
                        .or_else(|| entry.get("name"))
                        .and_then(|v| v.as_str().or_else(|| v.as_array()?.first()?.as_str()));
                    if token == Some(package) {
                        described.version = entry
                            .pointer("/versions/stable")
                            .or_else(|| entry.get("version"))
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                        if entry
                            .get("installed")
                            .map(|v| !v.as_str().unwrap_or("").is_empty())
                            .unwrap_or(false)
                            || entry
                                .get("installed")
                                .and_then(|v| v.as_array())
                                .map(|a| !a.is_empty())
                                .unwrap_or(false)
                        {
                            described.installed_version = described.version.clone();
                        }
                    }
                }
            }
        }
        Ok(described)
    }

    fn install(
        &self,
        _request: &InstallRequest,
        described: &PackageDescription,
    ) -> Result<(), SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "brew is not installed".to_owned(),
            ));
        };
        let is_cask = described.package.contains('/')
            || Command::new(binary)
                .args(["info", "--cask", "--json=v2", &described.package])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
        let mut args = vec!["install".to_owned()];
        if is_cask {
            args.push("--cask".to_owned());
        }
        args.push(described.package.clone());
        let output = Command::new(binary).args(&args).output().map_err(|e| {
            SoftwareError::ProviderFailed {
                provider: "brew".to_owned(),
                message: e.to_string(),
            }
        })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if stderr.to_lowercase().contains("sudo") || elevation_hint(&stderr) {
            // Homebrew itself refuses sudo; surface as elevation so the
            // runtime explains rather than retrying with privileges.
            return Err(SoftwareError::ElevationRequired {
                package: described.package.clone(),
                provider: "brew".to_owned(),
            });
        }
        Err(SoftwareError::ProviderFailed {
            provider: "brew".to_owned(),
            message: stderr.trim().to_owned(),
        })
    }

    fn update(&self, package: &str) -> Result<(), SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "brew is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["upgrade", package])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "brew".to_owned(),
                message: e.to_string(),
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(SoftwareError::ProviderFailed {
            provider: "brew".to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }

    fn uninstall(&self, package: &str) -> Result<(), SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "brew is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["uninstall", package])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "brew".to_owned(),
                message: e.to_string(),
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(SoftwareError::ProviderFailed {
            provider: "brew".to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Flatpak provider (Linux desktops and developer machines).
pub struct FlatpakProvider {
    binary: Option<PathBuf>,
}

impl FlatpakProvider {
    pub fn new() -> Self {
        Self {
            binary: find_binary("flatpak"),
        }
    }
}

impl Default for FlatpakProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for FlatpakProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Flatpak
    }

    fn available(&self) -> bool {
        self.binary.is_some()
    }

    fn search(&self, query: &str) -> Result<Vec<PackageDescription>, SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "flatpak is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["search", query])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "flatpak".to_owned(),
                message: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(SoftwareError::ProviderFailed {
                provider: "flatpak".to_owned(),
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .skip(1)
            .filter_map(|line| {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 3 {
                    Some(PackageDescription {
                        package: parts[2].trim().to_owned(),
                        name: parts[0].trim().to_owned(),
                        source: Some(
                            parts
                                .get(4)
                                .map(|s| (*s).trim().to_owned())
                                .unwrap_or_else(|| "flatpak".to_owned()),
                        ),
                        version: parts.get(3).map(|s| (*s).trim().to_owned()),
                        ..Default::default()
                    })
                } else {
                    None
                }
            })
            .collect())
    }

    fn describe_exact(&self, package: &str) -> Result<PackageDescription, SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "flatpak is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["info", package])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "flatpak".to_owned(),
                message: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(SoftwareError::NotFound(package.to_owned()));
        }
        let mut described = PackageDescription {
            package: package.to_owned(),
            name: package.to_owned(),
            source: Some("flatpak".to_owned()),
            installed_version: Some("installed".to_owned()),
            ..Default::default()
        };
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some((key, value)) = line.split_once(':') {
                match key.trim().to_lowercase().as_str() {
                    "version" => {
                        described.version = Some(value.trim().to_owned());
                        described.installed_version = Some(value.trim().to_owned());
                    }
                    "origin" => described.source = Some(value.trim().to_owned()),
                    _ => {}
                }
            }
        }
        Ok(described)
    }

    fn install(
        &self,
        _request: &InstallRequest,
        described: &PackageDescription,
    ) -> Result<(), SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "flatpak is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["install", "-y", "--noninteractive", &described.package])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "flatpak".to_owned(),
                message: e.to_string(),
            })?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if elevation_hint(&stderr) {
            return Err(SoftwareError::ElevationRequired {
                package: described.package.clone(),
                provider: "flatpak".to_owned(),
            });
        }
        Err(SoftwareError::ProviderFailed {
            provider: "flatpak".to_owned(),
            message: stderr.trim().to_owned(),
        })
    }

    fn update(&self, package: &str) -> Result<(), SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "flatpak is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["update", "-y", package])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "flatpak".to_owned(),
                message: e.to_string(),
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(SoftwareError::ProviderFailed {
            provider: "flatpak".to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }

    fn uninstall(&self, package: &str) -> Result<(), SoftwareError> {
        let Some(binary) = &self.binary else {
            return Err(SoftwareError::NoProvider(
                "flatpak is not installed".to_owned(),
            ));
        };
        let output = Command::new(binary)
            .args(["uninstall", "-y", package])
            .output()
            .map_err(|e| SoftwareError::ProviderFailed {
                provider: "flatpak".to_owned(),
                message: e.to_string(),
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(SoftwareError::ProviderFailed {
            provider: "flatpak".to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// PackageKit DBus provider. Preferred on Linux desktops because it routes
/// authentication through the desktop's polkit agent instead of a terminal
/// sudo prompt. Implemented via `gdbus` argv probing; when the session bus
/// or PackageKit is absent the provider reports unavailable honestly.
pub struct PackageKitProvider;

impl Provider for PackageKitProvider {
    fn id(&self) -> ProviderId {
        ProviderId::PackageKit
    }

    fn available(&self) -> bool {
        cfg!(target_os = "linux")
            && find_binary("gdbus").is_some()
            && Command::new("gdbus")
                .args([
                    "call",
                    "--session",
                    "--dest",
                    "org.freedesktop.PackageKit",
                    "--object-path",
                    "/org/freedesktop/PackageKit",
                    "--method",
                    "org.freedesktop.DBus.Properties.Get",
                    "org.freedesktop.PackageKit",
                    "VersionMajor",
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
    }

    fn search(&self, _query: &str) -> Result<Vec<PackageDescription>, SoftwareError> {
        Err(SoftwareError::Unsupported(
            "PackageKit search requires an async transaction client; use the distro or Flatpak provider for search and PackageKit for install/update".to_owned(),
        ))
    }

    fn describe_exact(&self, _package: &str) -> Result<PackageDescription, SoftwareError> {
        Err(SoftwareError::Unsupported(
            "PackageKit describe requires an async transaction client; use the distro or Flatpak provider for describe".to_owned(),
        ))
    }

    fn install(
        &self,
        _request: &InstallRequest,
        _described: &PackageDescription,
    ) -> Result<(), SoftwareError> {
        Err(SoftwareError::Unsupported(
            "PackageKit install requires the desktop polkit agent path; not yet wired in this build".to_owned(),
        ))
    }

    fn update(&self, _package: &str) -> Result<(), SoftwareError> {
        Err(SoftwareError::Unsupported(
            "PackageKit update requires the desktop polkit agent path; not yet wired in this build"
                .to_owned(),
        ))
    }

    fn uninstall(&self, _package: &str) -> Result<(), SoftwareError> {
        Err(SoftwareError::Unsupported(
            "PackageKit uninstall requires the desktop polkit agent path; not yet wired in this build".to_owned(),
        ))
    }
}

/// Check if a provider supports the given operation.
pub fn provider_supports(id: ProviderId, operation: &str) -> bool {
    match (id, operation) {
        (ProviderId::WinGet, "install")
        | (ProviderId::WinGet, "update")
        | (ProviderId::WinGet, "uninstall") => true,
        (ProviderId::Homebrew, "install")
        | (ProviderId::Homebrew, "update")
        | (ProviderId::Homebrew, "uninstall") => true,
        (ProviderId::Flatpak, "install")
        | (ProviderId::Flatpak, "update")
        | (ProviderId::Flatpak, "uninstall") => true,
        (ProviderId::Apt, "install")
        | (ProviderId::Apt, "update")
        | (ProviderId::Apt, "uninstall") => true,
        (ProviderId::Dnf, "install")
        | (ProviderId::Dnf, "update")
        | (ProviderId::Dnf, "uninstall") => true,
        (ProviderId::PackageKit, "install")
        | (ProviderId::PackageKit, "update")
        | (ProviderId::PackageKit, "uninstall") => false, // not yet wired
        (ProviderId::MacAppStore, _) => false, // not yet wired
        _ => false,
    }
}

/// Providers in V5 priority order for the current platform that support the given operation.
pub fn available_providers(operation: &str) -> Vec<ProviderId> {
    let mut providers = Vec::new();
    #[cfg(target_os = "windows")]
    {
        let winget = WinGetProvider::new();
        if winget.available() && provider_supports(ProviderId::WinGet, operation) {
            providers.push(ProviderId::WinGet);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let brew = HomebrewProvider::new();
        if brew.available() && provider_supports(ProviderId::Homebrew, operation) {
            providers.push(ProviderId::Homebrew);
        }
        // MacAppStore not yet wired for any operation
    }
    #[cfg(target_os = "linux")]
    {
        // PackageKit listed but marked as not supporting install/update/uninstall yet
        if PackageKitProvider.available() && provider_supports(ProviderId::PackageKit, operation) {
            providers.push(ProviderId::PackageKit);
        }
        let flatpak = FlatpakProvider::new();
        if flatpak.available() && provider_supports(ProviderId::Flatpak, operation) {
            providers.push(ProviderId::Flatpak);
        }
        if find_binary("apt").is_some() && provider_supports(ProviderId::Apt, operation) {
            providers.push(ProviderId::Apt);
        } else if find_binary("dnf").is_some() && provider_supports(ProviderId::Dnf, operation) {
            providers.push(ProviderId::Dnf);
        }
        let brew = HomebrewProvider::new();
        if brew.available() && provider_supports(ProviderId::Homebrew, operation) {
            providers.push(ProviderId::Homebrew);
        }
    }
    // Cross-platform fallback: Homebrew exists on all three OSes.
    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(not(target_os = "macos"))]
        {
            let brew = HomebrewProvider::new();
            if brew.available() && !providers.contains(&ProviderId::Homebrew) {
                providers.push(ProviderId::Homebrew);
            }
        }
    }
    providers
}

/// Select the provider for a request and operation: explicit choice when available,
/// otherwise the first available platform provider that supports the operation.
pub fn provider_for(
    requested: Option<ProviderId>,
    operation: &str,
) -> Result<Box<dyn Provider>, SoftwareError> {
    if let Some(id) = requested {
        let provider: Box<dyn Provider> = match id {
            ProviderId::WinGet => Box::new(WinGetProvider::new()),
            ProviderId::Homebrew => Box::new(HomebrewProvider::new()),
            ProviderId::PackageKit => Box::new(PackageKitProvider),
            ProviderId::Flatpak => Box::new(FlatpakProvider::new()),
            ProviderId::Apt | ProviderId::Dnf => {
                return Err(SoftwareError::Unsupported(
                    "distro package-manager installs are explicit-fallback only and not wired in this build".to_owned(),
                ));
            }
            ProviderId::MacAppStore => {
                return Err(SoftwareError::Unsupported(
                    "Mac App Store installs need a supported user-authenticated route; not wired in this build".to_owned(),
                ));
            }
        };
        if !provider.available() {
            return Err(SoftwareError::NoProvider(format!(
                "{} is not available on this machine",
                id.as_str()
            )));
        }
        return Ok(provider);
    }
    let available = available_providers(operation);
    let Some(first) = available.first() else {
        return Err(SoftwareError::NoProvider(format!(
            "no software provider is available on this platform for operation: {operation}",
        )));
    };
    provider_for(Some(*first), operation)
}
