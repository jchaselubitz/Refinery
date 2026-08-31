//! The case orchestrator: the state machine, case lifecycle operations, and
//! the rules that decide what runs next.
//!
//! The state machine is a pure `transition(state, transition) -> Result<state>`
//! function so every legal and illegal edge is table-testable without a
//! database. Filled in M1 (transitions) and M2 (orchestration over storage).

pub mod state_machine;
pub mod worker;

pub use state_machine::{is_legal, transition, TransitionError};
pub use worker::Worker;
