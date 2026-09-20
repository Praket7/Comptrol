use super::*;

#[test]
fn auth_popups_are_never_auto() {
    for class in [
        PopupClass::SystemAuthentication,
        PopupClass::InstallerElevation,
        PopupClass::SecurityWarning,
        PopupClass::PurchaseOrPayment,
        PopupClass::LegalAgreement,
        PopupClass::DestructiveConfirmation,
        PopupClass::PermissionRequest,
        PopupClass::BrowserPermission,
        PopupClass::Unknown,
    ] {
        assert!(class.never_auto(), "{class:?} must never auto-dismiss");
        assert!(!class.auto_eligible());
    }
}

#[test]
fn classifier_flags_auth_and_payment() {
    assert_eq!(
        classify(
            "dialog",
            "Authentication Required",
            "Enter your password to authenticate"
        ),
        PopupClass::SystemAuthentication
    );
    assert_eq!(
        classify(
            "dialog",
            "User Account Control",
            "Do you want to allow this app"
        ),
        PopupClass::InstallerElevation
    );
    assert_eq!(
        classify("dialog", "Checkout", "Enter payment details to buy now"),
        PopupClass::PurchaseOrPayment
    );
    assert_eq!(
        classify(
            "dialog",
            "License Agreement",
            "Accept the agreement to continue"
        ),
        PopupClass::LegalAgreement
    );
    assert_eq!(
        classify(
            "dialog",
            "Confirm",
            "Are you sure you want to delete 12 files"
        ),
        PopupClass::DestructiveConfirmation
    );
}

#[test]
fn unknown_content_never_auto_dismisses() {
    let popup = PopupInfo {
        id: "p1".to_owned(),
        class: classify("window", "???", "???"),
        title: None,
        target: "app".to_owned(),
        close_actions: vec!["OK".to_owned()],
        observed_at_ms: 0,
    };
    assert_eq!(popup.class, PopupClass::Unknown);
    let policy = PopupPolicy {
        dismiss_informational: true,
        dismiss_cookie_banners: true,
        dismiss_update_prompts: true,
        dismiss_tips: true,
    };
    assert!(authorize_dismissal(&popup, &policy).is_err());
}

#[test]
fn conservative_policy_dismisses_nothing() {
    let popup = PopupInfo {
        id: "p2".to_owned(),
        class: PopupClass::Informational,
        title: Some("Tip".to_owned()),
        target: "app".to_owned(),
        close_actions: vec!["Close".to_owned()],
        observed_at_ms: 0,
    };
    assert!(authorize_dismissal(&popup, &PopupPolicy::conservative()).is_err());
}

#[test]
fn semantic_action_beats_escape() {
    let popup = PopupInfo {
        id: "p3".to_owned(),
        class: PopupClass::Informational,
        title: None,
        target: "app".to_owned(),
        close_actions: vec!["Learn more".to_owned(), "Not Now".to_owned()],
        observed_at_ms: 0,
    };
    assert_eq!(
        DismissalPlan::for_popup(&popup),
        DismissalPlan::SemanticAction {
            action: "not now".to_owned()
        }
    );
    let bare = PopupInfo {
        close_actions: vec![],
        ..popup
    };
    assert_eq!(
        DismissalPlan::for_popup(&bare),
        DismissalPlan::ScopedKey {
            key: "Escape".to_owned()
        }
    );
}

#[test]
fn stack_tracks_per_target_depth() {
    let mut stack = PopupStack::new();
    assert_eq!(stack.depth("a"), 0);
    let popup = PopupInfo {
        id: "p".to_owned(),
        class: PopupClass::Informational,
        title: None,
        target: "a".to_owned(),
        close_actions: vec![],
        observed_at_ms: 0,
    };
    stack.push("a", popup);
    assert_eq!(stack.depth("a"), 1);
    assert_eq!(stack.depth("b"), 0);
    assert!(stack.pop("a").is_some());
    assert_eq!(stack.depth("a"), 0);
}
