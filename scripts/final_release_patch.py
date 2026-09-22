#!/usr/bin/env python3
from pathlib import Path

path = Path("crates/comptrol-core/src/lib.rs")
text = path.read_text(encoding="utf-8")

replacements = [
    (
        '            "accessibility" => "ms-settings:privacy-accessibility".to_owned(),\n',
        '            "accessibility" => return None,\n',
        "Windows accessibility permission route",
    ),
    (
        '            "status": if std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some() { "configured" } else { "not_configured" },\n',
        '            "status": if std::env::var_os("COMPTROL_CDP_ENDPOINT").is_some() { "direct_cdp_configured" } else if browser_bridge::bridge_is_active() { "companion_bridge_connected" } else { "not_configured" },\n',
        "browser capability status",
    ),
    (
        '            "extension": "experimental_only"\n',
        '            "extension": if browser_bridge::bridge_is_active() { "connected_authenticated" } else { "optional_not_connected" }\n',
        "Browser Bridge extension status",
    ),
    (
        '                    comptrol_browser::SessionProvider::CompanionExtension => "closed-group restore without full CDP",\n',
        '                    comptrol_browser::SessionProvider::CompanionExtension => "signed-in tabs, groups, and CDP through the authenticated native bridge",\n',
        "CompanionExtension preferred-for description",
    ),
]

for old, new, label in replacements:
    if old in text:
        text = text.replace(old, new, 1)
    elif new not in text:
        raise SystemExit(f"{label} pattern not found")

path.write_text(text, encoding="utf-8")
print("Applied final permission and Browser Bridge metadata truthfulness fixes")

# This file is intentionally deleted by the guarded workflow after success.
