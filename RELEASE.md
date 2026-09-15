# Release

The release candidate path is source build, format check, unit tests, README lint, package validation, browser and client conformance, and a manual doctor run on each supported operating system.

Homebrew formula generation is prepared in `scripts/generate_homebrew_formula.py` and is only valid after a release archive checksum exists.

The release workflow prepares native builds on Linux, macOS Intel, macOS Apple Silicon, and Windows. After native CI passes, it bundles those verified binaries into the `comptrolling` npm package and attaches a Homebrew formula that selects verified macOS and Linux archives by CPU architecture. The manually published initial Homebrew artifact now supports Apple Silicon and Intel macOS. Linux artifacts remain pending native Linux runners. A public tap requires a public formula and download location.

Each native package now has a checksum and Cargo metadata SBOM, and both the build job and publication job verify the checksums before release. `scripts/package_release.py` accepts an explicit operator supplied RSA signing key and emits a detached signature plus public key. `scripts/verify_release.py` verifies those signatures when present. No signing key is stored in this repository. GitHub artifact attestations are not enabled for this private repository because GitHub documents private repository attestation support as an Enterprise Cloud feature. Enable that only after the repository plan and release permissions support it.

An operator signing key is a private RSA key held by the release owner. It signs a release archive so users can verify that the file came from the release owner and was not replaced after publication. The private key must stay outside GitHub and outside this repository. Only the matching public key is distributed with the release. A release can be verified with `openssl dgst -sha256 -verify release-public-key.pem -signature comptrol-release.zip.sig comptrol-release.zip`. The repository includes a temporary signing conformance test, but it does not create or trust a production key automatically.
