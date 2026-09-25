#![deny(unsafe_code)]

pub const LEGACY_VERSION: &str = "2025-03-26";
pub const HANDSHAKE_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    LEGACY_VERSION,
    "2024-11-05",
];
pub const CURRENT_VERSION: &str = "2026-07-28";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolMode {
    Legacy,
    Current,
}

pub fn negotiate(requested: Option<&str>) -> Result<(&'static str, ProtocolMode), String> {
    match requested {
        Some(CURRENT_VERSION) => Ok((CURRENT_VERSION, ProtocolMode::Current)),
        Some(version) => HANDSHAKE_VERSIONS
            .iter()
            .copied()
            .find(|supported| *supported == version)
            .map(|supported| (supported, ProtocolMode::Legacy))
            .ok_or_else(|| format!("unsupported MCP protocol version {version}")),
        None => Ok((LEGACY_VERSION, ProtocolMode::Legacy)),
    }
}

pub fn from_header(value: Option<&str>) -> Result<ProtocolMode, String> {
    match value {
        Some(CURRENT_VERSION) => Ok(ProtocolMode::Current),
        Some(version) if HANDSHAKE_VERSIONS.contains(&version) => Ok(ProtocolMode::Legacy),
        Some(other) => Err(format!("unsupported MCP protocol version {other}")),
        None => Ok(ProtocolMode::Legacy),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_legacy_and_current_modes_without_accepting_unknown_versions() {
        assert_eq!(negotiate(None).unwrap().1, ProtocolMode::Legacy);
        for version in HANDSHAKE_VERSIONS {
            assert_eq!(negotiate(Some(version)).unwrap(), (*version, ProtocolMode::Legacy));
            assert_eq!(from_header(Some(version)).unwrap(), ProtocolMode::Legacy);
        }
        assert_eq!(
            negotiate(Some(CURRENT_VERSION)).unwrap().1,
            ProtocolMode::Current
        );
        assert_eq!(from_header(Some(CURRENT_VERSION)).unwrap(), ProtocolMode::Current);
        assert!(negotiate(Some("future")).is_err());
        assert!(from_header(Some("future")).is_err());
    }
}
