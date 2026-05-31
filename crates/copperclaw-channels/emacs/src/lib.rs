//! Emacs channel adapter — talks to a running Emacs daemon via
//! `emacsclient -e <sexp>`.
//!
//! # Elisp contract
//!
//! The user runs an Emacs server (`emacs --daemon` or `(server-start)` in
//! their init) and defines two functions in their Emacs setup. The
//! adapter never tries to be clever about Emacs internals: it shells out
//! to `emacsclient` and exchanges JSON-encoded strings.
//!
//! ## `copperclaw-pop-inbound`
//!
//! Returns the next queued inbound message, or `nil`. The adapter polls
//! this on a configurable interval.
//!
//! ```elisp
//! (defvar copperclaw--inbox nil
//!   "Queue of pending inbound messages for copperclaw.")
//!
//! (defun copperclaw-enqueue (buffer text &optional sender)
//!   "Queue (BUFFER TEXT SENDER) as an inbound message for copperclaw."
//!   (let ((entry `(("buffer" . ,buffer)
//!                  ("text"   . ,text))))
//!     (when sender
//!       (setq entry (append entry `(("sender" . ,sender)))))
//!     (setq copperclaw--inbox (append copperclaw--inbox (list entry)))))
//!
//! (defun copperclaw-pop-inbound ()
//!   "Pop and return the oldest queued inbound message, or nil."
//!   (when copperclaw--inbox
//!     (prog1 (car copperclaw--inbox)
//!       (setq copperclaw--inbox (cdr copperclaw--inbox)))))
//! ```
//!
//! ## `copperclaw-deliver`
//!
//! Receives a buffer name and text and decides how to surface them. A
//! minimal implementation appends to the buffer:
//!
//! ```elisp
//! (defun copperclaw-deliver (buffer text)
//!   "Append TEXT to BUFFER, creating it if needed."
//!   (with-current-buffer (get-buffer-create buffer)
//!     (goto-char (point-max))
//!     (insert text "\n")))
//! ```
//!
//! # What is intentionally not supported
//!
//! - File attachments. Where exactly to put a file inside Emacs is too
//!   user-specific to get right in a generic adapter; `deliver` returns
//!   [`AdapterError::Unsupported`](copperclaw_channels_core::AdapterError::Unsupported)
//!   for any outbound with `files`.
//! - System actions (`edit`, `reaction`). Likewise too platform-specific
//!   for v1.
//!
//! # Configuration
//!
//! See [`EmacsConfig`] for the JSON schema and defaults. The two template
//! tokens substituted into the outbound sexp template are:
//!
//! - `${BUFFER_JSON}` — JSON-encoded target buffer name.
//! - `${TEXT_JSON}` — JSON-encoded message body.

mod adapter;
mod client;
mod config;
mod factory;
mod sexp;

pub use adapter::{
    CHANNEL_TYPE_STR, EmacsAdapter, TOKEN_BUFFER_JSON, TOKEN_TEXT_JSON,
    build_inbound_from_pairs, render_outbound,
};
pub use client::{
    EmacsClient, EmacsClientCli, EmacsClientPlan, MockEmacsClient, classify_output,
};
pub use config::{
    DEFAULT_BUFFER, DEFAULT_CLIENT_BIN, DEFAULT_INBOUND_QUEUE_SEXP,
    DEFAULT_OUTBOUND_SEXP_TEMPLATE, DEFAULT_POLL_INTERVAL_MS, EmacsConfig,
};
pub use factory::{EmacsFactory, register};
pub use sexp::{ParseError, SexpValue};
