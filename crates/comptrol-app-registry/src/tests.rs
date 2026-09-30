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

#[test]
fn program_files_start_app_identity_resolves_only_inside_program_files() {
    let root = std::env::temp_dir().join(format!("comptrol-app-registry-{}", std::process::id()));
    let executable = root.join("Blender Foundation/Blender 5.2/blender-launcher.exe");
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::write(&executable, b"fixture executable").unwrap();
    let previous = std::env::var_os("ProgramFiles");
    unsafe { std::env::set_var("ProgramFiles", &root) };
    let resolved = registry::program_files_registration_path(
        r"{6D809377-6AF0-444B-8957-A3773F02200E}\Blender Foundation\Blender 5.2\blender-launcher.exe",
    );
    match previous {
        Some(value) => unsafe { std::env::set_var("ProgramFiles", value) },
        None => unsafe { std::env::remove_var("ProgramFiles") },
    }
    assert_eq!(resolved, Some(std::fs::canonicalize(&executable).unwrap()));
    assert!(
        registry::program_files_registration_path(
            r"{6D809377-6AF0-444B-8957-A3773F02200E}\..\Windows\System32\cmd.exe"
        )
        .is_none()
    );
    std::fs::remove_dir_all(root).unwrap();
}
