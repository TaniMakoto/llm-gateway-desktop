use crate::{SseTerminalState, StreamOutcome};

/// Tool assembly errors differ from a transport or explicit upstream error:
/// a declared token limit may legitimately leave a tool non-executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFailureKind {
    ToolCompletion,
    Upstream,
}

#[derive(Debug, Default)]
pub struct StreamLifecycle {
    limited: bool,
    completed: bool,
    cancelled: bool,
    failure: Option<StreamFailureKind>,
}

impl StreamLifecycle {
    pub fn observe_terminal(&mut self, terminal: SseTerminalState) {
        if terminal == SseTerminalState::Incomplete {
            self.limited = true;
        }
    }

    pub fn complete(&mut self) {
        self.completed = true;
    }
    pub fn cancel(&mut self) {
        if !self.completed {
            self.cancelled = true;
        }
    }
    pub fn fail(&mut self, kind: StreamFailureKind) {
        if self.failure != Some(StreamFailureKind::Upstream) {
            self.failure = Some(kind);
        }
    }
    pub fn has_failure(&self) -> bool {
        self.failure.is_some()
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
    pub fn outcome(&self) -> StreamOutcome {
        match self.failure {
            Some(StreamFailureKind::ToolCompletion) if self.limited => StreamOutcome::Incomplete,
            Some(_) => StreamOutcome::UpstreamFailure,
            None if self.cancelled || !self.completed => StreamOutcome::Cancelled,
            None if self.limited => StreamOutcome::Incomplete,
            None => StreamOutcome::Success,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limited_tool_is_not_executable_but_does_not_fail_provider_health() {
        for limit_first in [true, false] {
            let mut state = StreamLifecycle::default();
            if limit_first {
                state.observe_terminal(SseTerminalState::Incomplete);
            }
            state.fail(StreamFailureKind::ToolCompletion);
            if !limit_first {
                state.observe_terminal(SseTerminalState::Incomplete);
            }
            state.complete();
            assert!(state.has_failure());
            assert_eq!(state.outcome(), StreamOutcome::Incomplete);
            state.fail(StreamFailureKind::Upstream);
            state.observe_terminal(SseTerminalState::Incomplete);
            assert_eq!(state.outcome(), StreamOutcome::UpstreamFailure);
        }
    }

    #[test]
    fn an_unlimited_bad_tool_fails_and_late_error_wins_over_completion_or_cancel() {
        for complete_first in [true, false] {
            let mut state = StreamLifecycle::default();
            if complete_first {
                state.complete();
            } else {
                state.cancel();
            }
            state.fail(StreamFailureKind::ToolCompletion);
            assert_eq!(state.outcome(), StreamOutcome::UpstreamFailure);
            state.complete();
            assert_eq!(state.outcome(), StreamOutcome::UpstreamFailure);
        }
        let mut state = StreamLifecycle::default();
        state.complete();
        state.cancel();
        assert_eq!(state.outcome(), StreamOutcome::Success);
    }
}
