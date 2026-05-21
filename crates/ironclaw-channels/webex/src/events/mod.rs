//! Webex webhook ingress: HTTP router + payload parsing.

pub mod router;

pub use router::{EventDedup, WebexEventsState, WebexWebhookEnvelope, build_events_router};
