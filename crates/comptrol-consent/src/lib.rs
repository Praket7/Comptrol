//! Persistent capability and consent broker.
//!
//! The V4 policy layer is secure but depends on process environment
//! switches. This crate adds the persistent half of the V5 consent
//! design: durable, revocable, scoped grants that survive process
//! restarts, plus a human action broker for the moments when only the
//! user can approve something.
//!
//! Non-negotiable rules encoded here:
//! 1. Agent output or page content can never grant capability.
//! 2. A user instruction may authorize one bounded requested action when
//!    policy allows, and nothing broader.
//! 3. Broader grants are only created through the local setup/control
//!    surface, never through the agent tool channel.
//! 4. Grants may be scoped by app, resource, operation, session, or
//!    time, and every grant is independently revocable.
//! 5. Privilege elevation is never represented as a reusable credential.
//! 6. Revocation takes effect before the next dispatch: [`ConsentStore`]
//!    checks are the only path through which intents are authorized.

pub mod human_action;
pub mod scopes;
pub mod store;

pub use human_action::{
    HumanActionBroker, HumanActionChallenge, HumanActionRequest, HumanActionResolution,
    PendingHumanAction,
};
pub use scopes::{ConsentScope, GrantSubject};
pub use store::{ConsentGrant, ConsentStore, GrantConditions, StoreError};

/// Risks a grant may cover. Mirrors the core risk ladder so the consent
/// layer and the runtime policy layer fail closed together.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    R0,
    R1,
    R2,
    R3,
}

/// One decision about one intent on one resource.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum ConsentDecision {
    Allowed { grant_id: String },
    Denied { reason: String },
}

#[cfg(test)]
mod tests;
