#![deny(unsafe_code)]

pub const LEGACY_VERSION: &str = "2025-03-26";
pub const CURRENT_VERSION: &str = "2026-07-28";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolMode {
    Legacy,
    Current,
}

pub fn negotiate(requested: Option<&str>) -> Result<(&'static str, ProtocolMode), String> {
    match requested {
        None | Some(LEGACY_VERSION) => Ok((LEGACY_VERSION, ProtocolMode::Legacy)),
        Some(CURRENT_VERSION) => Ok((CURRENT_VERSION, ProtocolMode::Current)),
        Some(other) => Err(format!("unsupported MCP protocol version {other}")),
    }
}

pub fn from_header(value: Option<&str>) -> Result<ProtocolMode, String> {
    match value {
        None | Some(LEGACY_VERSION) => Ok(ProtocolMode::Legacy),
        Some(CURRENT_VERSION) => Ok(ProtocolMode::Current),
        Some(other) => Err(format!("unsupported MCP protocol version {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_legacy_and_current_modes_without_accepting_unknown_versions() {
        assert_eq!(negotiate(None).unwrap().1, ProtocolMode::Legacy);
        assert_eq!(
            negotiate(Some(CURRENT_VERSION)).unwrap().1,
            ProtocolMode::Current
        );
        assert!(negotiate(Some("future")).is_err());
    }
}
