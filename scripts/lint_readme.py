import re
from pathlib import Path

text = Path("README.md").read_text(encoding="utf8")
# Keep the project README within the requested plain language style.
text = re.sub(r"```.*?```", "", text, flags=re.DOTALL)
text = re.sub(r"`[^`]*`", "", text)
text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
for character in "-:;–—":
    if character in text:
        raise SystemExit(f"README contains forbidden character {character!r}")

print("README prose punctuation check passed")
