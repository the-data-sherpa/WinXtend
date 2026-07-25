//! One error type for every backend.
//!
//! Deliberately small and OS-agnostic. The engine above this crate has to make
//! the same decisions whether a Wayland portal denied a request or macOS has not
//! been granted Accessibility permission: retry, degrade the advertised
//! [`wx_proto::Capabilities`], or tell the user to grant something. Leaking a
//! per-OS error enum upwards would force that decision to be made five times.

/// Result alias used throughout the platform traits.
pub type Result<T> = core::result::Result<T, PlatformError>;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The backend for this OS cannot do this at all.
    ///
    /// Distinct from a runtime failure: the caller should stop advertising the
    /// corresponding capability rather than retrying. A headless evdev node
    /// reports this for clipboard access forever, not just today.
    #[error("{operation} is not supported by the {backend} backend")]
    Unsupported {
        operation: &'static str,
        backend: &'static str,
    },

    /// The OS refused for permission reasons the user can fix.
    ///
    /// Kept separate from [`PlatformError::Os`] because it is the one failure the
    /// UI must surface as an actionable prompt: macOS Accessibility and Input
    /// Monitoring, and the Wayland RemoteDesktop portal, all fail this way and
    /// all are silent no-ops otherwise.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// A raw OS call failed. `code` is the platform's own error number
    /// (`GetLastError`, `errno`, an `OSStatus`) kept verbatim so a bug report is
    /// searchable.
    #[error("{context} failed (os error {code})")]
    Os { context: &'static str, code: i32 },

    /// Another process owns the clipboard right now.
    ///
    /// Its own variant because it is transient and the correct response is to
    /// retry shortly. On Windows the clipboard is a single global lock and any
    /// application can hold it across a slow paint.
    #[error("clipboard is locked by another process")]
    ClipboardBusy,

    /// The clipboard holds nothing in the requested format.
    #[error("clipboard has no content in the requested format")]
    ClipboardEmpty,

    /// No usable display server was found, so this node cannot participate in
    /// the layout. Not fatal: a headless node still forwards input.
    #[error("no display server available")]
    NoDisplayServer,

    /// `start` was called on an already-running capture. Two live low-level hook
    /// chains would double every keystroke.
    #[error("input capture is already running")]
    AlreadyCapturing,

    #[error("input capture is not running")]
    NotCapturing,

    /// Content that is well-formed on the wire but unusable on this OS, e.g. a
    /// clipboard payload that is not valid UTF-8.
    #[error("malformed data: {0}")]
    Malformed(String),

    #[error("{0}")]
    Other(String),
}

impl PlatformError {
    /// Whether retrying the same call could plausibly succeed.
    ///
    /// The clipboard poller uses this to distinguish "try again in 50ms" from
    /// "stop advertising clipboard support", which otherwise degenerates into a
    /// hot retry loop against a permanent failure.
    pub fn is_transient(&self) -> bool {
        matches!(self, PlatformError::ClipboardBusy)
    }
}
