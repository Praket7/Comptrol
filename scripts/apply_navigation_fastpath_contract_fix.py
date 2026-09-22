#!/usr/bin/env python3
from pathlib import Path

path = Path(__file__).resolve().parents[1] / "crates/comptrol-core/src/lib.rs"
text = path.read_text()
old = '''                        json!({
                            "navigated": false,
                            "reason": "requested state already live on the exact target",
                            "ensure_state": state,
                            "mouse": "untouched",
                            "clipboard": "untouched",
                        }),
'''
new = '''                        json!({
                            "navigated": false,
                            "final_url": state.get("current_url").cloned().unwrap_or(Value::Null),
                            "reason": "requested state already live on the exact target",
                            "ensure_state": state,
                            "verification": {
                                "level": "surface_state",
                                "source": "browser_target_state",
                                "criterion": "final_url_matches_request",
                                "passed": true
                            },
                            "mouse": "untouched",
                            "clipboard": "untouched",
                        }),
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"expected one navigation fast-path response block, found {count}")
path.write_text(text.replace(old, new, 1))
print("navigation fast-path contract fixed")
