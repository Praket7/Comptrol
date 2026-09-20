import json
from pathlib import Path

text = Path("README.md").read_text(encoding="utf8")
for character in "-–—:;":
    if character in text:
        raise SystemExit(f"README contains forbidden character {character!r}")

# Generated consistency checks: the README must agree with the single VERSION
# source and the published npm package identity rather than drifting.
version = Path("VERSION").read_text(encoding="utf8").strip()
package = json.loads(Path("packages/mcp/package.json").read_text(encoding="utf8"))
if package.get("version") != version:
    raise SystemExit(
        f"README check: npm package version {package.get('version')!r} != VERSION {version!r}"
    )
name = package.get("name")
if name and name not in text:
    raise SystemExit(f"README does not reference the published npm package name {name!r}")

print("README punctuation and package consistency checks passed")
