//! Fence logic moved to the shared markdown renderer (M18/C5b).
//!
//! This module used to be authoritative for fence-state tracking — the
//! `scan_fence_spans` scanner and the `is_balanced` verdict the splitter
//! consulted, built by C2 with an explicit "the shared markdown renderer
//! is expected to absorb this" note. C5b honoured that: the scanner, the
//! `FenceKind`/`FenceSpan` model, and the fence-aware splitter now live in
//! [`copperclaw_channels_core::markdown`], the one place canonical
//! markdown turns into per-platform flavor. `service::split_text_into_chunks`
//! delegates to [`copperclaw_channels_core::markdown::split_into_chunks`];
//! this shim keeps the crate-internal `crate::fence::is_balanced` path the
//! splitter tests use, re-exported from core so those C2 tests still assert
//! balance through the migrated logic.

// Only the splitter tests reference this (asserting every emitted chunk
// balances); gate the re-export so non-test builds don't flag it unused.
#[cfg(test)]
pub(crate) use copperclaw_channels_core::markdown::is_balanced;
