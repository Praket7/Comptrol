use super::*;

#[test]
fn unknown_setting_names_do_not_parse() {
    assert!(SettingKey::from_name("registry.HKLM.Software.Evil").is_none());
    assert!(SettingKey::from_name("settings.security.disable").is_none());
}

#[test]
fn known_settings_round_trip() {
    for key in SettingKey::all() {
        let name = key.name();
        let parsed = SettingKey::from_name(&name).expect("declared setting must parse");
        assert_eq!(parsed.name(), name);
    }
}

#[test]
fn protected_settings_are_human_gated() {
    assert!(SettingKey::AccessibilityComptrolStatus.human_gated());
    assert!(
        SettingKey::PrivacyMicrophoneAppStatus {
            app: "x".to_owned()
        }
        .human_gated()
    );
    assert!(!SettingKey::DisplayBrightness.human_gated());
}

#[test]
fn human_gated_set_never_writes() {
    let result = set(
        &SettingKey::AccessibilityComptrolStatus,
        SettingValue::bool(true),
    );
    assert!(matches!(
        result,
        Err(SettingsError::HumanActionRequired { .. })
            | Err(SettingsError::UnsupportedPlatform { .. })
    ));
}
