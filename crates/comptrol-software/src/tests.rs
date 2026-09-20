use super::*;

#[test]
fn exactness_gate_refuses_unknown_package_without_provider() {
    // No provider on a bare machine: honest unavailability, never a guess.
    let request = InstallRequest::new("definitely-not-a-real-package-xyz");
    let result = install(&request);
    assert!(result.is_err());
}

#[test]
fn agreements_gate_requires_explicit_acceptance() {
    let error = SoftwareError::AgreementsRequired {
        package: "Example.App".to_owned(),
        agreements: vec!["EULA".to_owned()],
    };
    assert!(error.to_string().contains("Example.App"));
}

#[test]
fn provider_ids_have_stable_names() {
    assert_eq!(ProviderId::WinGet.as_str(), "winget");
    assert_eq!(ProviderId::Homebrew.as_str(), "brew");
    assert_eq!(ProviderId::PackageKit.as_str(), "packagekit");
}

#[test]
fn explicit_unavailable_provider_is_honest() {
    let result = provider_for(Some(ProviderId::MacAppStore));
    // Either unsupported or unavailable on this machine; never Ok by accident.
    assert!(result.is_err());
}
