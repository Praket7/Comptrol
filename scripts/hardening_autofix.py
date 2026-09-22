#!/usr/bin/env python3
from pathlib import Path

path = Path("crates/comptrol-core/src/browser_bridge.rs")
text = path.read_text(encoding="utf-8")

replacements = [
    (
        '''            Ok(mut file) => {\n                writeln!(file, "{token}")?;\n                file.sync_all()?;\n                return Ok(token);\n            }\n            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {\n                return auth_token(state_dir)?.ok_or_else(|| {\n                    io::Error::new(\n                        io::ErrorKind::InvalidData,\n                        "browser bridge token is invalid",\n                    )\n                });\n            }\n            Err(error) => return Err(error),''',
        '''            Ok(mut file) => {\n                writeln!(file, "{token}")?;\n                file.sync_all()?;\n                Ok(token)\n            }\n            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {\n                auth_token(state_dir)?.ok_or_else(|| {\n                    io::Error::new(\n                        io::ErrorKind::InvalidData,\n                        "browser bridge token is invalid",\n                    )\n                })\n            }\n            Err(error) => Err(error),''',
    ),
    (
        '''fn decode_hex(value: &str) -> io::Result<Vec<u8>> {\n    if value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {\n        return Err(io::Error::new(\n            io::ErrorKind::InvalidInput,\n            "invalid hexadecimal value",\n        ));\n    }\n    value\n        .as_bytes()\n        .chunks_exact(2)\n        .map(|pair| {\n            let text = std::str::from_utf8(pair)\n                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;\n            u8::from_str_radix(text, 16)\n                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))\n        })\n        .collect()\n}''',
        '''fn decode_hex(value: &str) -> io::Result<Vec<u8>> {\n    let bytes = value.as_bytes();\n    let (pairs, remainder) = bytes.as_chunks::<2>();\n    if !remainder.is_empty() || !bytes.iter().all(u8::is_ascii_hexdigit) {\n        return Err(io::Error::new(\n            io::ErrorKind::InvalidInput,\n            "invalid hexadecimal value",\n        ));\n    }\n    pairs\n        .iter()\n        .map(|pair| {\n            let text = std::str::from_utf8(pair)\n                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;\n            u8::from_str_radix(text, 16)\n                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))\n        })\n        .collect()\n}''',
    ),
]

for old, new in replacements:
    count = text.count(old)
    if count == 0:
        raise SystemExit(f"expected hardening pattern not found:\n{old[:180]}")
    text = text.replace(old, new)

path.write_text(text, encoding="utf-8")
print("Applied Browser Bridge Rust 1.98 clippy fixes")
