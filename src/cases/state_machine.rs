//! The case state machine as a pure transition function.
//!
//! Everything that moves a case goes through [`transition`]. It touches no
//! database, no clock, and no I/O, so every legal and illegal edge is
//! table-testable, and the storage layer's job reduces to: compute the next
//! state here, then write the snapshot and the event in one transaction.
//!
//! Three rules are worth stating because they are not obvious from the happy
//! path in the product description:
//!
//! - **`awaiting_answer -> running` repeats.** A case may need clarification
//!   any number of times; nothing in the machine counts question rounds.
//! - **A failed delivery attempt does not leave `delivering`.** The attempt is
//!   recorded on the delivery row and a retry is scheduled; only exhaustion or
//!   a non-retryable failure moves the case to `failed`, with the output
//!   preserved. `ready` is a transit state before the first attempt and is
//!   never returned to.
//! - **`failed` is left only by a person.** A user-initiated delivery retry is
//!   the single edge out of a terminal state, which is why cancellation is not
//!   legal from `failed`: there is nothing running to cancel.

use crate::domain::{CaseState, CaseTransition};

/// Why a transition was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransitionError {
    /// The transition is not legal from the case's current state.
    #[error("cannot apply {transition} to a case in state {from}: {reason}")]
    Illegal {
        /// The state the case was in.
        from: CaseState,
        /// The transition that was attempted, as a wire string.
        transition: &'static str,
        /// Why it is not legal.
        reason: &'static str,
    },
}

impl TransitionError {
    fn illegal(from: CaseState, transition: &CaseTransition, reason: &'static str) -> Self {
        TransitionError::Illegal {
            from,
            transition: transition.as_str(),
            reason,
        }
    }
}

/// Apply a transition to a state.
///
/// Returns the resulting state, or [`TransitionError::Illegal`] naming what was
/// attempted and why it was refused. The refusal reason is part of the value,
/// not a log line, because it is what the API returns when a client cancels a
/// case that already completed or retries a delivery that never had an output.
pub fn transition(
    state: CaseState,
    transition: &CaseTransition,
) -> Result<CaseState, TransitionError> {
    use CaseState::*;
    use CaseTransition as T;

    match transition {
        // Cancellation is legal from every non-terminal state and from nowhere
        // else. Checked first so it needs no arm in the state match below.
        T::Cancelled { .. } => {
            if state.is_terminal() {
                Err(TransitionError::illegal(
                    state,
                    transition,
                    "the case has already stopped",
                ))
            } else {
                Ok(Cancelled)
            }
        }

        // Failure is likewise legal from anywhere the case is still live. A
        // case that already stopped cannot fail again; re-recording it would
        // overwrite the real reason it stopped.
        T::Failed { .. } => {
            if state.is_terminal() {
                Err(TransitionError::illegal(
                    state,
                    transition,
                    "the case has already stopped",
                ))
            } else {
                Ok(Failed)
            }
        }

        T::PreparationStarted => match state {
            Received => Ok(Preparing),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "preparation starts only from a freshly received case",
            )),
        },

        T::BackendStarted => match state {
            Preparing => Ok(Running),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "the backend starts only after preparation",
            )),
        },

        T::QuestionRaised { .. } => match state {
            Running => Ok(AwaitingAnswer),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "only a running backend can raise a question",
            )),
        },

        // The repeatable edge: a case may go round the question loop as many
        // times as the agent needs.
        T::AnswersApplied { .. } => match state {
            AwaitingAnswer => Ok(Running),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "answers apply only to a case that is waiting for them",
            )),
        },

        T::OutputSubmitted { .. } => match state {
            Running => Ok(Validating),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "only a running backend can submit an output",
            )),
        },

        T::ValidationSucceeded { .. } => match state {
            Validating => Ok(Ready),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "validation applies only to a submitted output",
            )),
        },

        // The agent's one corrective retry hands the case back to the backend
        // with the validation issues. Exhausting that retry is a `Failed`
        // transition, not another rejection.
        T::ValidationRejectedWithRetry { .. } => match state {
            Validating => Ok(Running),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "validation applies only to a submitted output",
            )),
        },

        // `ready` for the first attempt; `failed` when a person retries a
        // delivery whose output is still preserved.
        T::DeliveryStarted { .. } => match state {
            Ready | Failed => Ok(Delivering),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "delivery starts from a validated output or a user-initiated retry",
            )),
        },

        // A failed attempt with a retry scheduled keeps the case in place.
        T::DeliveryAttemptFailed { .. } => match state {
            Delivering => Ok(Delivering),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "only an in-flight delivery can record a failed attempt",
            )),
        },

        T::DeliverySucceeded { .. } => match state {
            Delivering => Ok(Completed),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "only an in-flight delivery can succeed",
            )),
        },

        T::DeliveryExhausted { .. } => match state {
            Delivering => Ok(Failed),
            _ => Err(TransitionError::illegal(
                state,
                transition,
                "only an in-flight delivery can exhaust its attempts",
            )),
        },
    }
}

/// Whether a transition is legal from a state, without applying it.
///
/// Used by the API to answer "can this case be cancelled?" without attempting
/// the write, and by the interface to decide which actions to offer.
pub fn is_legal(state: CaseState, candidate: &CaseTransition) -> bool {
    transition(state, candidate).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{DeliveryId, OutputId, QuestionRequestId};
    use crate::error::RetryClass;
    use CaseState::*;

    fn question() -> CaseTransition {
        CaseTransition::QuestionRaised {
            question_request_id: QuestionRequestId::new(),
        }
    }

    fn answers() -> CaseTransition {
        CaseTransition::AnswersApplied {
            question_request_id: QuestionRequestId::new(),
        }
    }

    fn submitted() -> CaseTransition {
        CaseTransition::OutputSubmitted {
            output_id: OutputId::new(),
        }
    }

    fn validated() -> CaseTransition {
        CaseTransition::ValidationSucceeded {
            output_id: OutputId::new(),
        }
    }

    fn rejected() -> CaseTransition {
        CaseTransition::ValidationRejectedWithRetry {
            output_id: OutputId::new(),
        }
    }

    fn delivery_started() -> CaseTransition {
        CaseTransition::DeliveryStarted {
            delivery_id: DeliveryId::new(),
        }
    }

    fn attempt_failed() -> CaseTransition {
        CaseTransition::DeliveryAttemptFailed {
            delivery_id: DeliveryId::new(),
            retry_class: RetryClass::Retryable,
        }
    }

    fn delivered() -> CaseTransition {
        CaseTransition::DeliverySucceeded {
            delivery_id: DeliveryId::new(),
        }
    }

    fn exhausted() -> CaseTransition {
        CaseTransition::DeliveryExhausted {
            delivery_id: DeliveryId::new(),
        }
    }

    fn failed() -> CaseTransition {
        CaseTransition::Failed {
            reason: "provider refused the request".into(),
        }
    }

    fn cancelled() -> CaseTransition {
        CaseTransition::Cancelled { reason: None }
    }

    /// Every edge the product describes, as a table.
    fn legal_edges() -> Vec<(CaseState, CaseTransition, CaseState)> {
        vec![
            (Received, CaseTransition::PreparationStarted, Preparing),
            (Preparing, CaseTransition::BackendStarted, Running),
            (Running, question(), AwaitingAnswer),
            (AwaitingAnswer, answers(), Running),
            (Running, submitted(), Validating),
            (Validating, validated(), Ready),
            (Validating, rejected(), Running),
            (Ready, delivery_started(), Delivering),
            (Delivering, attempt_failed(), Delivering),
            (Delivering, delivered(), Completed),
            (Delivering, exhausted(), Failed),
            (Failed, delivery_started(), Delivering),
            (Received, failed(), Failed),
            (Preparing, failed(), Failed),
            (Running, failed(), Failed),
            (AwaitingAnswer, failed(), Failed),
            (Validating, failed(), Failed),
            (Ready, failed(), Failed),
            (Delivering, failed(), Failed),
            (Received, cancelled(), Cancelled),
            (Preparing, cancelled(), Cancelled),
            (Running, cancelled(), Cancelled),
            (AwaitingAnswer, cancelled(), Cancelled),
            (Validating, cancelled(), Cancelled),
            (Ready, cancelled(), Cancelled),
            (Delivering, cancelled(), Cancelled),
        ]
    }

    /// Every transition, once each, for exhaustive sweeps.
    fn all_transitions() -> Vec<CaseTransition> {
        vec![
            CaseTransition::PreparationStarted,
            CaseTransition::BackendStarted,
            question(),
            answers(),
            submitted(),
            validated(),
            rejected(),
            delivery_started(),
            attempt_failed(),
            delivered(),
            exhausted(),
            failed(),
            cancelled(),
        ]
    }

    #[test]
    fn every_legal_edge_lands_where_the_product_says() {
        for (from, edge, expected) in legal_edges() {
            assert_eq!(
                transition(from, &edge),
                Ok(expected),
                "{from} --{}--> expected {expected}",
                edge.as_str()
            );
        }
    }

    #[test]
    fn every_edge_outside_the_table_is_refused() {
        let legal: Vec<(CaseState, &'static str)> = legal_edges()
            .iter()
            .map(|(from, edge, _)| (*from, edge.as_str()))
            .collect();

        for state in CaseState::ALL {
            for edge in all_transitions() {
                let expected_legal = legal.contains(&(*state, edge.as_str()));
                let result = transition(*state, &edge);
                assert_eq!(
                    result.is_ok(),
                    expected_legal,
                    "{state} --{}--> got {result:?}",
                    edge.as_str()
                );
            }
        }
    }

    #[test]
    fn the_question_loop_repeats_without_limit() {
        let mut state = Running;
        for _ in 0..5 {
            state = transition(state, &question()).unwrap();
            assert_eq!(state, AwaitingAnswer);
            state = transition(state, &answers()).unwrap();
            assert_eq!(state, Running);
        }
    }

    #[test]
    fn the_full_happy_path_runs_end_to_end() {
        let path = [
            (CaseTransition::PreparationStarted, Preparing),
            (CaseTransition::BackendStarted, Running),
            (question(), AwaitingAnswer),
            (answers(), Running),
            (submitted(), Validating),
            (validated(), Ready),
            (delivery_started(), Delivering),
            (delivered(), Completed),
        ];
        let mut state = CaseState::INITIAL;
        for (edge, expected) in path {
            state = transition(state, &edge).unwrap();
            assert_eq!(state, expected);
        }
        assert!(state.is_terminal());
    }

    #[test]
    fn a_failed_delivery_attempt_stays_in_delivering() {
        let mut state = Delivering;
        for _ in 0..3 {
            state = transition(state, &attempt_failed()).unwrap();
            assert_eq!(state, Delivering);
        }
        assert_eq!(transition(state, &exhausted()), Ok(Failed));
    }

    #[test]
    fn a_person_may_retry_delivery_from_failed_but_nothing_else() {
        assert_eq!(transition(Failed, &delivery_started()), Ok(Delivering));
        assert!(transition(Failed, &cancelled()).is_err());
        assert!(transition(Failed, &failed()).is_err());
        assert!(transition(Failed, &submitted()).is_err());
    }

    #[test]
    fn ready_is_never_returned_to_once_delivery_starts() {
        for edge in all_transitions() {
            if let Ok(next) = transition(Delivering, &edge) {
                assert_ne!(next, Ready, "{} returned to ready", edge.as_str());
            }
        }
    }

    #[test]
    fn cancellation_reaches_every_non_terminal_state_and_no_terminal_one() {
        for state in CaseState::ALL {
            assert_eq!(
                is_legal(*state, &cancelled()),
                !state.is_terminal(),
                "cancellation from {state}"
            );
        }
    }

    #[test]
    fn completed_and_cancelled_accept_nothing_at_all() {
        for state in [Completed, Cancelled] {
            for edge in all_transitions() {
                assert!(
                    transition(state, &edge).is_err(),
                    "{state} accepted {}",
                    edge.as_str()
                );
            }
        }
    }

    #[test]
    fn a_refusal_names_the_state_the_transition_and_the_reason() {
        let err = transition(Completed, &cancelled()).unwrap_err();
        let TransitionError::Illegal {
            from,
            transition: name,
            reason,
        } = &err;
        assert_eq!(*from, Completed);
        assert_eq!(*name, "cancelled");
        assert!(!reason.is_empty());
        assert!(err.to_string().contains("completed"));
    }

    #[test]
    fn transitions_never_land_on_a_state_that_still_holds_a_job_from_awaiting_answer() {
        // A case waiting on a person holds no job, so the only way out is an
        // answer, a failure, or a cancellation. Anything else would strand a
        // case with nothing scheduled to move it.
        let reachable: Vec<&'static str> = all_transitions()
            .iter()
            .filter(|edge| transition(AwaitingAnswer, edge).is_ok())
            .map(|edge| edge.as_str())
            .collect();
        assert_eq!(reachable, vec!["answers_applied", "failed", "cancelled"]);
    }
}
