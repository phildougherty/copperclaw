//! `copperclaw-setup` — interactive first-time setup for an copperclaw host.
//!
//! See `PLAN.md` § 6 (T10). The crate ships a single binary
//! (`copperclaw-setup`) that walks an operator through environment checks,
//! data-directory creation, central-DB migration, container-image build,
//! optional vault wiring, credential capture, mount allow-listing, service
//! unit generation, and a smoke test against the freshly created DB. Every
//! step is a small `Step` impl driven by a `Prompt` abstraction so the same
//! code path serves interactive, headless (env-var driven), and scripted
//! (test-canned) execution.
//!
//! ## Crate layout
//!
//! - [`config`]: typed outputs of the steps.
//! - [`state`]: JSON state persisted between runs.
//! - [`prompt`]: `Prompt` trait + interactive / env-backed / scripted impls.
//! - [`steps`]: `Step` trait + every step in its own module.
//! - [`units`]: systemd unit and launchd plist generators.
//! - [`migrator`]: data-directory migrator (`--migrate-from`).

#![forbid(unsafe_code)]

pub mod cli;
pub mod config;
pub mod migrator;
pub mod prompt;
pub mod state;
pub mod steps;
pub mod units;
