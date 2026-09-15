# Homebrew packaging

The formula is generated only from published archives and their verified SHA256 values. The release workflow prepares separate archives for macOS Apple Silicon, macOS Intel, Linux ARM64, and Linux x64.

```text
python3 scripts/generate_homebrew_formula.py --version VERSION --macos-arm64-url ARM64_MAC_URL --macos-arm64-sha256 ARM64_MAC_SHA --macos-intel-url INTEL_MAC_URL --macos-intel-sha256 INTEL_MAC_SHA --linux-arm64-url ARM64_LINUX_URL --linux-arm64-sha256 ARM64_LINUX_SHA --linux-intel-url INTEL_LINUX_URL --linux-intel-sha256 INTEL_LINUX_SHA --formula-name Comptrolling --output Formula/comptrolling.rb
```

No release archive is claimed until the native binary and checksum have been verified. The current private `v0.1.0` release contains Apple Silicon and Intel macOS artifacts. Linux artifacts will be added by the native release matrix when GitHub runners are available. A public Homebrew tap also requires a public formula and download location.
