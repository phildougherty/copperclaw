# Adding a channel

This guide is for contributors writing a new channel adapter for
ironclaw. It walks through the contract surface, the crate layout, the
inbound and outbound mapping you must get right, the error variants,
configuration, container contributions, testing, and host wiring.

The reference implementations to read alongside this guide are:

- `crates/ironclaw-channels/cli/` — the smallest possible adapter,
  shipped as the in-tree default.
- `crates/ironclaw-channels/telegram/` — HTTP long-poll plus webhook,
  REST sends, `reqwest` + `axum` + `wiremock`.
- `crates/ironclaw-channels/slack/` — events API plus Web API, signed
  request verification, structured-message rendering.
- `crates/ironclaw-channels/discord/` — gateway websocket via
  `tokio-tungstenite`, REST sends.

By the time you are done, the new crate will compile, pass its own
tests, and the host will start it on boot via `boot::run_host`.

## 1. The contract

Two traits in `ironclaw-channels-core` define everything a channel must
implement.

### `ChannelAdapter`

```rust
#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn channel_type(&self) -> &ChannelType;

    fn supports_threads(&self) -> bool { false }

    async fn subscribe(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
    ) -> Result<(), AdapterError> { Ok(()) }

    async fn set_typing(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
    ) -> Result<(), AdapterError> { Ok(()) }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError>;

    async fn open_dm(&self, user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        Ok(None)
    }
}
```

The only required method is `deliver`. Override the others if your
platform supports them.

### `ChannelFactory`

```rust
#[async_trait]
pub trait ChannelFactory: Send + Sync {
    fn channel_type(&self) -> ChannelType;
    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError>;
    async fn shutdown(&self) -> Result<(), AdapterError> { Ok(()) }
    fn container_contribution(&self) -> ContainerContribution { ContainerContribution::default() }
}
```

The factory is the long-lived registry entry. `init` is called once per
configured instance of your channel; everything stateful lives in the
adapter the factory returns.

## 2. Crate layout

Every channel crate follows the same shape. Create a new directory at
`crates/ironclaw-channels/<name>/` and add it to the workspace
`Cargo.toml`.

### `Cargo.toml`

```toml
[package]
name = "ironclaw-channels-<name>"
edition.workspace = true
license.workspace = true
rust-version.workspace = true
version.workspace = true

[dependencies]
ironclaw-types = { workspace = true }
ironclaw-channels-core = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
async-trait = { workspace = true }
reqwest = { workspace = true }     # if you talk HTTP
serde = { workspace = true }
serde_json = { workspace = true }
chrono = { workspace = true }
thiserror = { workspace = true }
url = { workspace = true }
uuid = { workspace = true }

# Add per-platform deps here. Examples:
#   axum = { workspace = true }              # for inbound webhook
#   tokio-tungstenite = "..."                # for gateway websockets
#   bytes / sha2 / hex                       # for signature verification

[dev-dependencies]
wiremock = "=0.6.2"
tempfile = "3"

[lints]
workspace = true
```

### `src/lib.rs`

The public surface is exactly two items (per PLAN.md § 6 (T6)):

```rust
pub struct <Name>Factory;
impl ChannelFactory for <Name>Factory { /* ... */ }

pub fn register(reg: &mut ChannelRegistry) -> Result<(), AdapterError> {
    reg.register(Arc::new(<Name>Factory))
}
```

Anything else lives in private modules (`config.rs`, `events.rs`,
`signature.rs`, etc.). Do not export adapter types or transport
helpers; the host only needs the factory + the register fn.

### Suggested module split

For anything more complex than the CLI channel:

- `lib.rs` — `<Name>Factory`, `register`, public re-exports (none if
  you can help it).
- `config.rs` — `Config` struct + parser from
  `serde_json::Value` to a typed shape.
- `adapter.rs` — `<Name>Adapter` plus the `ChannelAdapter` impl.
- `events.rs` or `events/` — inbound parsing (one file per event kind
  if there are many).
- `signature.rs` — signature verification helpers for webhook adapters.

Keep the parser pure (`fn parse(payload: &serde_json::Value) ->
Result<InboundEvent, ParseError>`) so it can be unit-tested without
any HTTP dependencies.

## 3. Inbound mapping

When a platform sends you an event, you build an `InboundEvent` and
push it to the mpsc sender the host gave you in `ChannelSetup`. The
struct is in `ironclaw-types::message`:

```rust
pub struct InboundEvent {
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub thread_id: Option<String>,
    pub message: InboundMessage,
    pub reply_to: Option<ReplyTo>,
    pub sender: Option<SenderIdentity>,
}

pub struct InboundMessage {
    pub id: String,
    pub kind: MessageKind,
    pub content: serde_json::Value,
    pub timestamp: DateTime<Utc>,
    pub is_mention: Option<bool>,
    pub is_group: Option<bool>,
}
```

### Field-by-field

- `channel_type` — `ChannelType::new("yourchannel")`. Must match the
  factory's `channel_type()`.
- `platform_id` — the channel-side identifier of the conversation
  (chat id, channel id, room id). Pair it with `channel_type` and the
  host's `messaging_groups` table to resolve the wiring.
- `thread_id` — if the platform exposes threads, the thread id;
  otherwise `None`. Slack supplies `thread_ts`; Discord supplies the
  thread's snowflake; Telegram has topic ids; most others have `None`.
- `message.id` — the platform's message id, **as a string**. Different
  type from `ironclaw_types::MessageId`. Used by the host for
  idempotency keys.
- `message.kind` — usually `MessageKind::Chat`. Webhook adapters set
  `MessageKind::Webhook`. System / Task / Agent are reserved for
  host-internal use.
- `message.content` — a JSON object describing the body. Convention:
  `{ "text": "...", optional "attachments": [...] }`. If the platform
  carries richer structure (e.g. quoted replies, embeds), pass it
  through under sub-keys; the agent's prompt formatter will decide
  what to surface.
- `message.timestamp` — UTC. Most platforms report a Unix timestamp;
  convert with `DateTime::<Utc>::from_timestamp`.
- `message.is_mention` — true when the platform message addressed
  the agent. Compute this at the adapter (parse `<@U_agent>` for
  Slack, `@yourbot` for Telegram, `<@id>` for Discord). Leave `None`
  if you have not computed it.
- `message.is_group` — true when the chat is a group / channel rather
  than a 1-1 DM. Combine with `is_mention` to drive the wiring's
  engage mode (`pattern` / `mention` / `mention-sticky`).
- `reply_to` — populate when the inbound event was synthesised by the
  host on behalf of a different channel (rare in real adapters).
- `sender.identity` — channel-specific user identifier
  (`U01ABC`, `12345678`, `alice#1234`). Combined with `channel_type`
  forms the unique platform user key the host turns into a `UserId`
  (deterministic UUIDv5).
- `sender.display_name` — best-effort human-readable name. May be
  `None` if the platform does not surface one in the payload.

### Pushing the event

```rust
if let Err(err) = self.inbound_tx.send(event).await {
    tracing::warn!(?err, "inbound channel closed");
}
```

The host owns the receiver. If the channel is full it will back
pressure; if the receiver is dropped, the host is shutting down and
your adapter should drain quietly.

## 4. Outbound mapping

`deliver` is called by the host's delivery loop for each row in the
session's `outbound.db.messages_out`. You receive an
`OutboundMessage`:

```rust
pub struct OutboundMessage {
    pub kind: MessageKind,
    pub content: serde_json::Value,
    pub files: Vec<OutboundFile>,
}
```

### Text path

If `content` is `{ "text": "..." }` and `files` is empty, you are
sending a plain text message. Translate that to the platform's
"send message" API.

### File path

If `files` is non-empty, you must include each attachment. Each
`OutboundFile` carries `filename` and `data: Vec<u8>`. Upload first
where the platform demands it (Slack), or send as multipart in the
same call (Telegram), or attach to a message create (Discord).

Apply the same `safe_attachment_name()` rules the host already
enforces; defensive duplication is fine.

### Structured `content`

When the agent calls `send_card`, the outbound row's `content` is
the card payload verbatim. Recognise card shapes for your platform
and call the appropriate API:

- Slack: `{ "blocks": [...] }` → `chat.postMessage` with blocks.
- Telegram: `{ "text": "...", "buttons": [[...]] }` →
  `sendMessage` with `reply_markup`.
- Discord: `{ "embeds": [...], "components": [...] }` →
  `POST /channels/{id}/messages`.

If you cannot render the card shape, render a textual fallback so
the agent's intent reaches the user even imperfectly.

### System actions: edit and reaction

The host also calls `deliver` for outbound rows whose `kind` is
`System` and whose content describes an action:

```json
{ "action": "edit", "target_seq": 7, "text": "new body" }
{ "action": "reaction", "target_seq": 7, "emoji": "thumbsup" }
```

You translate `edit` to the platform's edit-message API and
`reaction` to the platform's reaction API. If the platform does not
support either, return `AdapterError::Unsupported(String)` so the
delivery loop can stop retrying.

### Return value

`deliver` returns `Ok(Some(platform_message_id))` when the platform
emits an id; `Ok(None)` otherwise. The host stores the id in
`delivered.platform_message_id` so admins and future tools can
correlate.

## 5. Error mapping

Map every platform failure to one of `AdapterError`'s variants:

| Platform symptom | `AdapterError` |
|---|---|
| HTTP 401 / 403 | `Auth(String)` |
| HTTP 429 with retry-after | `Rate { retry_after: Some(secs) }` |
| HTTP 429 without retry-after | `Rate { retry_after: None }` |
| HTTP 400 / 422 | `BadRequest(String)` |
| HTTP 5xx / connection failure | `Transport(String)` |
| `tokio::io::Error` | `Io(_)` (via `?`) |
| Platform says "this conv doesn't support X" | `Unsupported(String)` |
| You haven't implemented the trait method | `NotImplemented` |

The host's delivery loop reads these to decide whether to retry. See
the `error-handling` skill for the full retry table.

Do **not** invent new errors by jamming them into `BadRequest`; the
delivery loop uses the variant to choose retry policy, so the
classification matters.

## 6. Configuration

The host hands you a `ChannelSetup` at `init`:

```rust
pub struct ChannelSetup {
    pub config: serde_json::Value,
    pub inbound_tx: Sender<InboundEvent>,
    pub data_dir: PathBuf,
}
```

- `config` is whatever JSON the admin put in the channel's central-DB
  row. Parse it into a typed `Config` struct in your `config.rs`. Be
  strict — return `AdapterError::BadRequest` on unknown fields or
  type mismatches. The CLI channel's `CliConfig::from_value` is a
  good template.
- `inbound_tx` is the only sender you get. Stash a clone on the
  adapter; spawn whatever background tasks (long-poll, gateway
  reader, webhook server) you need; have each task push into the
  sender.
- `data_dir` is a stable host-side directory you may use freely
  (credential caches, attachment scratch, session resume tokens).
  The host guarantees it exists and is unique per channel instance.

Validate everything up front: a misconfigured channel should fail at
`init` with a clear `AdapterError::BadRequest` message, not silently
later.

## 7. Container contributions

`ContainerContribution::default()` (returned by the trait default) is
fine if your channel needs nothing inside the agent's container. But
many channels do: a Telegram channel might need a CA bundle, a Slack
channel might need an OAuth token mounted in, an X channel might need
the `playwright-mcp` npm package.

```rust
pub struct ContainerContribution {
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub packages_apt: Vec<String>,
    pub packages_npm: Vec<String>,
}
```

Examples:

- `env` — `("MY_CHANNEL_TOKEN", "<value>")` to make a token visible
  to the runner.
- `mounts` — `Mount::ro("/host/data/x/certs", "/etc/x-ca")` to add a
  CA bundle.
- `packages_apt` — `vec!["libssl-dev".into()]` to ensure the runtime
  has an SSL library.
- `packages_npm` — `vec!["some-cli".into()]` to add a tool the agent
  will shell out to.

The container runtime merges contributions from every wired channel,
fingerprints the result, and reuses or builds an image accordingly.

## 8. Testing strategy

Tests live in `src/` alongside the code and (optionally)
`tests/` for cross-module integration. The workspace's policy
(PLAN.md § 9) is full coverage of every public fn, type variant,
and error path.

### Pure parsers

For inbound parsing, keep the function pure:

```rust
pub(crate) fn parse_message(value: &serde_json::Value) -> Result<InboundEvent, ParseError>;
```

…and unit-test with captured fixtures. The Slack and Telegram crates
both ship fixtures under `src/events/`. Avoid involving HTTP in
parser tests.

### REST adapters

For platforms that talk HTTP, use `wiremock` to fake the API:

```rust
let server = wiremock::MockServer::start().await;
wiremock::Mock::given(method("POST"))
    .and(path("/bot<token>/sendMessage"))
    .respond_with(ResponseTemplate::new(200).set_body_json(...))
    .mount(&server)
    .await;
```

Construct the adapter with the test base URL (your `Config` should
allow an override). Assert that `deliver` issued the right call and
returned the expected `Ok(Some(message_id))`.

### Webhook adapters

For Slack-style adapters that mount an `axum` router, use
`tower::ServiceExt::oneshot` to drive the router without a TCP
socket. Inject the inbound mpsc receiver and assert the resulting
`InboundEvent`.

### Gateway adapters

For Discord-style websocket adapters, isolate the protocol parser
from the socket. The parser takes opcoded JSON in and emits events;
test that in isolation. The socket layer is exercised by integration
tests against a fake gateway.

### Error mapping

For every `AdapterError` variant your code can produce, write a
test that exercises the branch. The delivery loop changes behaviour
based on the variant, so a misclassification is a real bug.

## 9. Wiring into the host

The host's `boot::run_host` calls `channels_init::build_registry` to
construct a `ChannelRegistry`. Add your factory there:

```rust
// crates/ironclaw-host/src/channels_init.rs
pub fn build_registry() -> ChannelRegistry {
    let mut reg = ChannelRegistry::new();
    if let Err(err) = ironclaw_channels_cli::register(&mut reg) {
        tracing::warn!(?err, "failed to register cli factory");
    }
    if let Err(err) = ironclaw_channels_<name>::register(&mut reg) {
        tracing::warn!(?err, "failed to register <name> factory");
    }
    reg
}
```

…and add the new crate as a dependency in `ironclaw-host/Cargo.toml`.
Boot is intentionally non-fatal on duplicate registrations: if you
launch a host with two crates registering the same `ChannelType`,
the host logs and skips rather than crashing.

After registration, an admin enables an instance via the host
configuration:

```toml
[[channels]]
channel_type = "<name>"
config = { token = "...", base_url = "..." }
```

…and via `iclaw messaging-groups create --channel-type <name>
--platform-id <id> --name "Some chat"` plus
`iclaw wirings create --mg <mg> --ag <ag> --engage <mode> [--pattern <re>]`.

## 10. Checklist

Before opening a PR, walk through this list:

- `cargo build -p ironclaw-channels-<name>` passes.
- `cargo clippy -p ironclaw-channels-<name> --all-targets -- -D warnings`
  is clean.
- `cargo test -p ironclaw-channels-<name>` covers every variant of
  `AdapterError` your code emits.
- `Config::from_value` rejects unknown fields and bad types with
  `AdapterError::BadRequest` and a clear message.
- The inbound parser is pure (no async, no HTTP) and has fixture
  tests.
- `register()` is the only public function besides the factory type.
- The host's `build_registry()` calls `<crate>::register(&mut reg)`.
- The adapter's `channel_type()` matches the factory's
  `channel_type()` (the registry will catch this at runtime, but
  catching it in code review is better).
- `ContainerContribution` lists every env / mount / package the
  agent container actually needs.
- Tests run without network access (use `wiremock`).

Once these are green, the channel is ready to ship — and an
admin can add it to a deployment without touching code beyond the
configuration.
