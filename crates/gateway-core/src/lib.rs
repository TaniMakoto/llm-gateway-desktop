//! Protocol completion rules, independent of HTTP, Tauri and persistence.
mod lifecycle;
mod terminal;
mod tool_completion;
mod tool_stream;

pub use lifecycle::{StreamFailureKind, StreamLifecycle};
pub use terminal::{classify_terminal_event, incomplete_reason, SseTerminalState};
pub use tool_completion::{validate_tool_completion, ToolArgumentFormat};
pub use tool_stream::ToolCompletionObserver;

/// The final response outcome after both upstream reading and conversion finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    Success,
    /// An explicit protocol limit, rather than an unexpected transport EOF.
    Incomplete,
    UpstreamFailure,
    /// The consumer disappeared without an observed upstream failure.
    Cancelled,
}
