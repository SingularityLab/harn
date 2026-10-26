//! IO-free detection and reaction engine for terminal coding agents.
//!
//! Adapters render the agent's terminal into a [`Snapshot`], feed it to an [`Engine`] and execute the
//! returned [`Action`]s. Time is always injected, so everything here is deterministic.

pub mod classify;
pub mod engine;
pub mod policy;
pub mod profile;

pub use classify::{Classification, Problem, Trigger, classify};
pub use engine::{Action, Engine};
pub use policy::{Policy, PolicyError, Reaction, ReactionKind, Reactions};
pub use profile::{Profile, ProfileError, Signatures};

/// Rendered terminal screen as plain text (no ANSI), lines joined by `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub text: String,
    /// Epoch milliseconds of the last byte the agent printed.
    pub last_output_ms: u64,
}
