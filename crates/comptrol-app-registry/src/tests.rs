use super::*;

#[test]
fn resource_defaults_to_none() {
    assert_eq!(Resource::default(), Resource::None);
}

#[test]
fn launch_verification_labels_exit_early() {
    // PID 0 and PID 4_000_000 do not exist; the probe must distinguish
    // "exited" from "cannot check" on platforms with liveness support.
    let probe = launcher_probe(u32::MAX - 1);
    assert!(matches!(
        probe,
        launch::LaunchVerification::ExitedEarly | launch::LaunchVerification::Unavailable
    ));
}

#[test]
fn url_resources_require_native_open() {
    assert!(registry::requires_native_open(&Resource::Url {
        url: "https://example.com".into()
    }));
    assert!(registry::requires_native_open(&Resource::DeepLink {
        uri: "vscode://file/tmp".into()
    }));
    assert!(!registry::requires_native_open(&Resource::None));
}
