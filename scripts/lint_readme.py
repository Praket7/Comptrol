from pathlib import Path

text = Path("README.md").read_text(encoding="utf8")
for character in "-–—:;":
    if character in text:
        raise SystemExit(f"README contains forbidden character {character!r}")
print("README punctuation check passed")

