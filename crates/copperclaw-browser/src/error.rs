//! Error type for the headless-browser tool.

use thiserror::Error;

/// Every fallible browser operation returns this.
#[derive(Debug, Error)]
pub enum BrowserError {
    /// The browser tool is disabled (the opt-in flag is off — the default).
    /// Returned by the gating check so the caller can surface a clear "not
    /// enabled" message rather than attempting a spawn.
    #[error("browser tool is disabled (opt-in); set the enable flag to use it")]
    Disabled,

    /// The navigation target (or a redirect hop) was rejected by the SSRF
    /// guard. Carries the guard's human-readable reason.
    #[error("navigation blocked: {0}")]
    Blocked(String),

    /// The request was malformed (empty URL, unsupported render mode, etc.).
    #[error("invalid request: {0}")]
    Invalid(String),

    /// The headless browser / CDP driver failed (navigation timeout, crash,
    /// protocol error). Carries the driver's message.
    #[error("render failed: {0}")]
    Driver(String),

    /// The child container could not be provisioned (runtime unavailable,
    /// image missing, spawn error).
    #[error("browser container error: {0}")]
    Container(String),
}
