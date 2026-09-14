# Release

The release candidate path is source build, format check, unit tests, README lint, package validation, browser and client conformance, and a manual doctor run on each supported operating system.

Homebrew formula generation is prepared in `scripts/generate_homebrew_formula.py` and is only valid after a release archive checksum exists.

The release workflow prepares native builds on Linux, macOS, and Windows. Npm publication, signing, attestations, and a public cross platform benchmark remain future release work.

Each native package now has a checksum and Cargo metadata SBOM, and both the build job and publication job verify the checksums before release. GitHub artifact attestations are not enabled for this private repository because GitHub documents private repository attestation support as an Enterprise Cloud feature. Enable that only after the repository plan and release permissions support it.
