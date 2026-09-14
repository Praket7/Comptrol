# Homebrew packaging

The formula is generated only from a published archive and its verified SHA256 value.

```text
python3 scripts/generate_homebrew_formula.py --version VERSION --url ARCHIVE_URL --sha256 SHA256 --output Formula/comptrol.rb
```

No release archive is claimed until the native binary and checksum have been verified.

