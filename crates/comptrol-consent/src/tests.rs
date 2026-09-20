use super::*;

#[test]
fn risk_ladder_is_ordered() {
    assert!(Risk::R0 < Risk::R1);
    assert!(Risk::R1 < Risk::R2);
    assert!(Risk::R2 < Risk::R3);
}

#[test]
fn decisions_carry_ids_or_reasons() {
    assert!(matches!(
        ConsentDecision::Allowed {
            grant_id: "g".into()
        },
        ConsentDecision::Allowed { .. }
    ));
    assert!(matches!(
        ConsentDecision::Denied {
            reason: String::new()
        },
        ConsentDecision::Denied { .. }
    ));
}
