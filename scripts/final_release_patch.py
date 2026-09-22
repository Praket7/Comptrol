#!/usr/bin/env python3
from pathlib import Path

path = Path("crates/comptrol-core/src/lib.rs")
text = path.read_text(encoding="utf-8")
old = '            "accessibility" => "ms-settings:privacy-accessibility".to_owned(),\n'
new = '            "accessibility" => return None,\n'
if old in text:
    text = text.replace(old, new, 1)
elif new not in text:
    raise SystemExit("Windows accessibility permission route pattern not found")
path.write_text(text, encoding="utf-8")
print("Removed undocumented Windows accessibility settings URI")
