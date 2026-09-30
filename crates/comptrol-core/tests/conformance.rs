//! P4.5 conformance: proves the three registries that describe Comptrol's
//! surface cannot drift apart.
//!
//! Three independent facts used to be maintained by hand, and a real live
//! bug came from letting two of them disagree:
//!
//! 1. `CORE_INTENTS` / `FIRST_PARTY_ADAPTER_INTENTS` — what `operate`
//!    actually dispatches.
//! 2. `capability_catalog()` — what clients are told they can call.
//! 3. `Policy::from_gates` — what the local policy will authorize.
//!
//! An intent present in (1) but absent from (2) is invisible to clients; an
//! intent advertised in (2) but absent from (3) returns `policy_denied` at
//! call time. That exact combination shipped once
//! (`browser.ensure_session`), so every relationship is asserted here
//! rather than re-reviewed by hand each time an intent is added.

use comptrol::{
    CAPABILITY_FAMILIES, CORE_INTENTS, Capability, EnvGates, FIRST_PARTY_ADAPTER_INTENTS,
    GateRequirement, PLATFORM_OBSERVATIONS, Policy, Risk, callable_intent_catalog,
    capability_catalog, classify, intent_schema, route_catalog,
};
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// Discovery is expensive on some platforms (macOS Accessibility probes,
/// platform broker round trips), so build the catalog once per test binary.
fn capability_rows() -> &'static Vec<Capability> {
    static ROWS: OnceLock<Vec<Capability>> = OnceLock::new();
    ROWS.get_or_init(callable_intent_catalog)
}

fn catalog_names() -> BTreeSet<String> {
    capability_rows()
        .iter()
        .map(|capability| capability.name.clone())
        .collect()
}

/// Names the catalog reports as callable operate intents: everything except
/// platform observations and route-family labels.
fn callable_names() -> BTreeSet<String> {
    capability_rows()
        .iter()
        .map(|capability| capability.name.clone())
        .filter(|name| {
            !PLATFORM_OBSERVATIONS.contains(&name.as_str())
                && !CAPABILITY_FAMILIES.contains(&name.as_str())
        })
        .collect()
}

fn dispatchable_names() -> BTreeSet<String> {
    CORE_INTENTS
        .iter()
        .chain(FIRST_PARTY_ADAPTER_INTENTS.iter())
        .map(|intent| (*intent).to_owned())
        .collect()
}

/// Every dispatchable core intent is advertised in the catalog. Without
/// this, a route can be fully implemented and still be undiscoverable.
#[test]
fn every_dispatchable_core_intent_is_advertised() {
    let advertised = callable_names();
    let missing: Vec<&str> = CORE_INTENTS
        .iter()
        .copied()
        .filter(|intent| !advertised.contains(*intent))
        .collect();
    assert!(
        missing.is_empty(),
        "dispatched but not advertised in capability_catalog: {missing:?}"
    );
}

/// Nothing is advertised as callable unless dispatch actually handles it.
/// `platform.*` rows are excluded because they are observations, not
/// operations.
#[test]
fn nothing_is_advertised_without_a_dispatch_arm() {
    let dispatchable = dispatchable_names();
    let ghosts: Vec<String> = callable_names()
        .into_iter()
        .filter(|name| !dispatchable.contains(name))
        .collect();
    assert!(
        ghosts.is_empty(),
        "advertised as callable but never dispatched: {ghosts:?}"
    );
}

/// The catalog keeps route families and platform observations out of the
/// callable set, so a client cannot mistake a label for an intent name.
#[test]
fn capability_families_are_never_callable_intents() {
    let advertised = callable_names();
    let names = catalog_names();
    let catalog = capability_catalog();
    let reported = catalog["capability_families"]
        .as_array()
        .expect("capability_families section")
        .iter()
        .filter_map(|row| row["name"].as_str().map(str::to_owned))
        .collect::<BTreeSet<String>>();
    for family in CAPABILITY_FAMILIES {
        assert!(
            !advertised.contains(*family),
            "{family} is a family label but is reported as a callable intent"
        );
        assert!(
            !CORE_INTENTS.contains(family),
            "{family} is a family label but also appears in CORE_INTENTS"
        );
        if names.contains(*family) {
            assert!(
                reported.contains(*family),
                "{family} exists as a capability but is missing from capability_families"
            );
        }
    }
}

/// The regression this file exists for: an intent that no env-gate
/// combination will ever authorize, even though the catalog advertises it.
#[test]
fn every_gate_gated_intent_is_reachable_through_its_declared_gates() {
    let defaults = Policy::default();
    for intent in CORE_INTENTS {
        let required = GateRequirement::for_intent(intent, EnvGates::default());
        if required.is_empty() {
            assert!(
                defaults.allowed_intents.contains(*intent),
                "{intent} needs no gate, so it must be in the default allowlist"
            );
            continue;
        }
        // Build exactly the gate set the declaration names, and nothing
        // else, so a gate that only works in combination still fails.
        let mut gates = EnvGates::default();
        for var in required {
            gates = EnvGates::with(var, true)
                .unwrap_or_else(|| panic!("{intent} declares unknown policy gate {var}"));
        }
        let policy = Policy::from_gates(gates);
        assert!(
            policy.allowed_intents.contains(*intent),
            "{intent} is gated behind {required:?} but that combination does not allow it"
        );
        assert!(
            policy.authorize(intent, classify(intent)).is_ok(),
            "{intent} is allowlisted by {required:?} but authorize() still refuses it \
             (risk ceiling too low for {:?})",
            classify(intent)
        );
    }
}

/// The other half of the same bug class: a single switch must not unlock an
/// intent whose declared gate list does not include it, otherwise the
/// "required gates" a client is told about are a lie.
#[test]
fn single_switch_allowlists_match_the_declared_gate_requirements() {
    for (var, _) in EnvGates::NAMES {
        let policy = Policy::from_gates(EnvGates::with(var, true).expect("known gate"));
        for intent in &policy.allowed_intents {
            let declared = GateRequirement::for_intent(intent, EnvGates::default());
            let explained = declared.is_empty()
                || declared.contains(var)
                || *var == "COMPTROL_ALLOW_ALL_INTENTS";
            assert!(
                explained,
                "{intent} is unlocked by {var} but declares {declared:?}"
            );
        }
    }
}

/// Every intent the most permissive policy can reach is dispatchable. A
/// stale allowlist entry (renamed or deleted intent) would otherwise sit in
/// the policy forever, silently widening `max_risk` with no route behind it.
#[test]
fn the_permissive_policy_reaches_nothing_undispatchable() {
    let policy = Policy::from_gates(EnvGates::all());
    let known = dispatchable_names();
    let stale: Vec<String> = policy
        .allowed_intents
        .iter()
        .filter(|intent| !known.contains(intent.as_str()))
        .cloned()
        .collect();
    assert!(
        stale.is_empty(),
        "policy allows intents that are neither core nor adapter intents: {stale:?}"
    );
}

/// A published schema must describe an intent that exists, and its example
/// must validate against its own schema. A stale schema sends the model to
/// a route that no longer exists; a broken example is how the live
/// `invalid_input` on `limit`/`revision` shipped.
#[test]
fn published_schemas_are_live_and_self_consistent() {
    let callable = callable_names();
    for intent in intent_schema::available_schemas() {
        let schema = intent_schema::schema_for(intent)
            .unwrap_or_else(|| panic!("{intent} is published but has no schema"));
        assert_eq!(
            schema["intent"], intent,
            "schema for {intent} reports a different intent name"
        );
        assert!(
            callable.contains(intent),
            "{intent} has a published schema but is not an advertised callable intent"
        );
        let example = &schema["example"]["params"];
        assert!(
            !example.is_null(),
            "{intent} schema has no example params to validate"
        );
        intent_schema::validate_params(intent, example)
            .unwrap_or_else(|error| panic!("{intent} example fails its own schema: {error}"));
    }
}

/// Schema completeness (M4): a dispatchable core intent that no client can
/// discover is effectively private. `workflow.speculate` shipped
/// dispatch-complete but schema-less, so it was invisible to a client that
/// only reads published schemas.
#[test]
fn every_core_intent_is_discoverable_and_nothing_else_is_published() {
    let undocumented: Vec<&str> = CORE_INTENTS
        .iter()
        .copied()
        .filter(|intent| intent_schema::schema_for(intent).is_none())
        .collect();
    assert!(
        undocumented.is_empty(),
        "core intents without a published schema (M4): {undocumented:?}"
    );
    for intent in intent_schema::available_schemas() {
        assert!(
            dispatchable_names().contains(intent),
            "{intent} publishes a schema but is neither a core nor an adapter intent"
        );
    }
}

/// The route catalog and the intent registry describe the same surface to
/// clients, so a route entry for a nonexistent intent is a dead end.
#[test]
fn route_catalog_entries_are_dispatchable() {
    let known = dispatchable_names();
    for plan in route_catalog() {
        assert!(
            known.contains(plan.intent.as_str()),
            "route catalog advertises {} which is not dispatchable",
            plan.intent
        );
    }
}

/// `comptrol setup` and the docs read the gate list from `EnvGates::NAMES`,
/// so every declared gate must map to a real field, appear exactly once,
/// and combine without losing reach relative to the all-switches policy the
/// test suite exercises.
#[test]
fn the_gate_name_table_is_complete_unique_and_monotone() {
    let mut seen = BTreeSet::new();
    for (var, field) in EnvGates::NAMES {
        assert!(
            seen.insert(*var),
            "{var} is listed twice in EnvGates::NAMES"
        );
        assert!(
            seen.insert(*field),
            "{field} is listed twice in EnvGates::NAMES"
        );
        assert!(
            var.starts_with("COMPTROL_ALLOW_"),
            "policy gate {var} is not a COMPTROL_ALLOW_ variable"
        );
        assert!(
            EnvGates::with(var, true).is_some(),
            "{var} has no matching EnvGates field"
        );
    }
    let all = Policy::from_gates(EnvGates::all());
    for (var, _) in EnvGates::NAMES {
        let single = Policy::from_gates(EnvGates::with(var, true).unwrap());
        for intent in &single.allowed_intents {
            assert!(
                all.allowed_intents.contains(intent),
                "{intent} is reachable via {var} alone but missing from the all-gates policy"
            );
        }
        assert!(
            all.max_risk >= single.max_risk,
            "{var} alone allows risk {:?} but the all-gates policy only allows {:?}",
            single.max_risk,
            all.max_risk
        );
    }
}

/// Risk classification must be total, and the permissive policy must be
/// able to authorize every advertised intent. Otherwise the catalog
/// advertises routes that no configuration can reach.
#[test]
fn permissive_policy_authorizes_every_advertised_intent() {
    let policy = Policy::from_gates(EnvGates::all());
    assert_eq!(policy.max_risk, Risk::R3);
    let unreachable: Vec<String> = callable_names()
        .into_iter()
        .filter(|intent| policy.authorize(intent, classify(intent)).is_err())
        .collect();
    assert!(
        unreachable.is_empty(),
        "advertised intents no gate combination can authorize: {unreachable:?}"
    );
}
