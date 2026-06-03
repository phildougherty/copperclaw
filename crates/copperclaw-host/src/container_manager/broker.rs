//! Credential broker v1 (Phase 0b).
//!
//! ## The threat this closes
//!
//! Before this module the spawn path forwarded the long-lived provider key
//! (`ANTHROPIC_API_KEY`) straight into the container env (see
//! [`super::spawn::build_spec`], the `with_env("ANTHROPIC_API_KEY", ...)`
//! call). Any shell inside the container — including one driven by a
//! prompt-injected agent — could `printenv ANTHROPIC_API_KEY` and exfiltrate
//! the master key, which then bills the operator's account from anywhere,
//! forever, until the operator notices and rotates it.
//!
//! The broker is a **host-side authenticating model proxy**. It holds the
//! real key, listens on loopback, and the container is handed:
//!
//!   - `ANTHROPIC_BASE_URL = http://<broker-loopback>` (the runner already
//!     reads this to point its provider client at a custom base), and
//!   - `ANTHROPIC_API_KEY = <per-session capability token>` (the runner reads
//!     this into the `x-api-key` / `Authorization` slot — so the token rides
//!     the exact slot the master key used to occupy, with no runner change).
//!
//! When the runner makes a model call, it hits the broker with the token.
//! The broker validates the token, checks the group's budget, **swaps the
//! token for the real key host-side**, forwards upstream, and meters the
//! egress. The master key is never written to any container env, so a shell
//! `printenv` yields only the short-lived, group-scoped, revocable token.
//!
//! ## What this stops, and what it does NOT
//!
//! The broker stops key **theft**: an attacker who pops the container cannot
//! walk away with a credential that bills the account from elsewhere. The
//! token is scoped to one session+group, expires on a TTL, and the operator
//! can revoke it instantly (per-session or group-wide) without touching the
//! master key.
//!
//! The broker does **not** stop key **misuse** from inside the container: an
//! injected agent that still has a live token can spend the group's budget
//! through the broker for as long as the token is valid. That residual is
//! bounded by the per-group budget gate (enforced AT the broker here, in
//! addition to the spawn-time gate) and by token expiry/revocation — but a
//! live token is, by design, a bearer credential for *that group's spend*.
//! This is an honest, deliberate boundary: brokering moves the key out of
//! reach; it does not make the agent trustworthy.
//!
//! ## Default behaviour is unchanged
//!
//! The broker is **opt-in**. With `COPPERCLAW_CREDENTIAL_BROKER` unset (the
//! default), [`BrokerConfig::from_env`] returns `None`, the spawn path
//! forwards the real key exactly as before, and no loopback listener starts.
//! Only when an operator sets `COPPERCLAW_CREDENTIAL_BROKER=1` does the spawn
//! path swap to minting tokens.
//!
//! ## Testable core vs. deferred runtime
//!
//! Everything in this module that decides *whether* a request is allowed is
//! pure and unit-tested: token mint/validate/expiry/revocation
//! ([`BrokerKeyring`]), the per-group budget check ([`BudgetVerdict`]), the
//! end-to-end authz decision ([`authorize`]), and the upstream header
//! injection ([`UpstreamRequest::with_injected_auth`]). The live loopback
//! HTTP listener ([`serve`]) wires those decisions to a real socket; its
//! networking is the runtime path and is guarded behind the opt-in flag, so
//! the default install is byte-for-byte unaffected.

use copperclaw_types::{AgentGroupId, SessionId};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Length of the random signing secret, in bytes (256-bit HMAC key).
const SIGNING_KEY_LEN: usize = 32;

/// Token wire prefix + version. Bumping this invalidates every previously
/// minted token (they no longer parse), which is the desired behaviour on a
/// breaking format change.
const TOKEN_PREFIX: &str = "cct1";

/// Default capability-token lifetime when the operator doesn't override it.
/// Long enough to cover a multi-turn build (the runner re-reads the same
/// token for every call in a session) but short enough that a leaked token
/// expires within the hour rather than billing forever.
pub const DEFAULT_TOKEN_TTL_SECS: u64 = 3600;

/// Claims carried inside a capability token. Recovered (and authenticated)
/// by [`BrokerKeyring::validate`]. Everything here is signed — a tampered
/// field fails the HMAC check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenClaims {
    /// Session the token was minted for.
    pub session_id: SessionId,
    /// Agent group the session belongs to. Used to scope the budget check
    /// and the egress metering at the broker.
    pub agent_group_id: AgentGroupId,
    /// Unix epoch seconds at which the token was issued. Used for group-wide
    /// "revoke everything minted before T" sweeps.
    pub issued_at: u64,
    /// Unix epoch seconds after which the token is no longer valid.
    pub expires_at: u64,
}

/// Why a token failed validation. Distinguished so the broker can return the
/// right HTTP status (all map to 401/403, but the variant drives logging and
/// the metric `outcome` label) and so tests can assert the precise reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The string didn't have the `cct1.<payload>.<sig>` shape, or a field
    /// failed to parse.
    #[error("malformed token")]
    Malformed,
    /// The HMAC signature didn't match — the token was forged or tampered.
    #[error("bad token signature")]
    BadSignature,
    /// `now >= expires_at`.
    #[error("token expired")]
    Expired,
    /// The session id (or its group, via the watermark) was revoked.
    #[error("token revoked")]
    Revoked,
}

/// In-memory revocation state. Two mechanisms:
///
///   - **Per-session**: an explicit set of revoked [`SessionId`]s
///     (`cclaw sessions delete` / a future `broker revoke` would add here).
///   - **Group-wide watermark**: a per-group "minimum issued-at" — any token
///     issued *before* the watermark is rejected. Bumping the watermark to
///     `now` instantly invalidates every outstanding token for the group
///     without enumerating them (used for "rotate this group's access").
///
/// Process-local: a host restart clears it. That's acceptable because a
/// restart also stops every container and re-mints fresh tokens on respawn,
/// and the master key never left the host regardless.
#[derive(Debug, Default)]
pub struct Revocations {
    sessions: std::collections::HashSet<SessionId>,
    group_min_issued_at: HashMap<AgentGroupId, u64>,
}

impl Revocations {
    /// Empty revocation set — nothing revoked.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Revoke a single session's token(s).
    pub fn revoke_session(&mut self, session_id: SessionId) {
        self.sessions.insert(session_id);
    }

    /// Revoke every token for `group` issued strictly before `as_of`
    /// (unix epoch seconds). Monotonic: a later call with a smaller value
    /// does not lower the watermark.
    pub fn revoke_group_before(&mut self, group: AgentGroupId, as_of: u64) {
        let slot = self.group_min_issued_at.entry(group).or_insert(0);
        *slot = (*slot).max(as_of);
    }

    /// Whether the given claims are revoked under the current state.
    #[must_use]
    pub fn is_revoked(&self, claims: &TokenClaims) -> bool {
        if self.sessions.contains(&claims.session_id) {
            return true;
        }
        match self.group_min_issued_at.get(&claims.agent_group_id) {
            Some(&watermark) => claims.issued_at < watermark,
            None => false,
        }
    }
}

/// Holds the signing secret and mints/validates capability tokens. Cheap to
/// construct; clone the secret bytes only at construction. The secret never
/// leaves the host process and is never written to disk or a container.
#[derive(Clone)]
pub struct BrokerKeyring {
    signing_key: [u8; SIGNING_KEY_LEN],
}

impl std::fmt::Debug for BrokerKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the signing key.
        f.debug_struct("BrokerKeyring")
            .field("signing_key", &"<redacted>")
            .finish()
    }
}

impl BrokerKeyring {
    /// Build a keyring from a fixed 32-byte signing secret. Used by tests for
    /// determinism and by [`Self::generate`] internally.
    #[must_use]
    pub fn from_secret(signing_key: [u8; SIGNING_KEY_LEN]) -> Self {
        Self { signing_key }
    }

    /// Generate a keyring with a fresh random signing secret (CSPRNG). Called
    /// once at host boot when the broker is enabled.
    #[must_use]
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut key = [0u8; SIGNING_KEY_LEN];
        rand::rngs::OsRng.fill_bytes(&mut key);
        Self { signing_key: key }
    }

    /// Mint a capability token for a session valid for `ttl` from `issued_at`.
    /// The returned string is `cct1.<hex payload>.<hex hmac>` — opaque to the
    /// runner, which just stuffs it into the `x-api-key` slot.
    #[must_use]
    pub fn mint(
        &self,
        session_id: SessionId,
        agent_group_id: AgentGroupId,
        issued_at: u64,
        ttl: Duration,
    ) -> String {
        let claims = TokenClaims {
            session_id,
            agent_group_id,
            issued_at,
            expires_at: issued_at.saturating_add(ttl.as_secs()),
        };
        let payload = encode_payload(&claims);
        let sig = self.sign(payload.as_bytes());
        format!("{TOKEN_PREFIX}.{payload}.{}", hex::encode(sig))
    }

    /// Validate a token end to end: parse, verify HMAC, check expiry against
    /// `now` (unix epoch seconds), then check the revocation set. Returns the
    /// authenticated claims on success.
    ///
    /// Signature verification uses a constant-time compare so a timing
    /// side-channel can't be used to forge a signature byte-by-byte.
    pub fn validate(
        &self,
        token: &str,
        now: u64,
        revocations: &Revocations,
    ) -> Result<TokenClaims, TokenError> {
        let mut parts = token.split('.');
        let prefix = parts.next().ok_or(TokenError::Malformed)?;
        let payload = parts.next().ok_or(TokenError::Malformed)?;
        let sig_hex = parts.next().ok_or(TokenError::Malformed)?;
        if parts.next().is_some() || prefix != TOKEN_PREFIX {
            return Err(TokenError::Malformed);
        }
        let provided_sig = hex::decode(sig_hex).map_err(|_| TokenError::Malformed)?;
        let expected_sig = self.sign(payload.as_bytes());
        if !constant_time_eq(&provided_sig, &expected_sig) {
            return Err(TokenError::BadSignature);
        }
        // Signature is good; the payload is authentic, so parse it.
        let claims = decode_payload(payload).ok_or(TokenError::Malformed)?;
        if now >= claims.expires_at {
            return Err(TokenError::Expired);
        }
        if revocations.is_revoked(&claims) {
            return Err(TokenError::Revoked);
        }
        Ok(claims)
    }

    /// HMAC-SHA256 over `msg` with the signing key. Hand-rolled (RFC 2104) so
    /// the broker depends only on `sha2`, already in the workspace — no new
    /// `hmac` crate. The construction is the textbook one:
    /// `H((K' ^ opad) || H((K' ^ ipad) || msg))`.
    fn sign(&self, msg: &[u8]) -> [u8; 32] {
        const BLOCK: usize = 64; // SHA-256 block size.
        // Key is already < BLOCK, so K' = key padded with zeros to BLOCK.
        let mut k_block = [0u8; BLOCK];
        k_block[..SIGNING_KEY_LEN].copy_from_slice(&self.signing_key);

        let mut ipad = [0x36u8; BLOCK];
        let mut opad = [0x5cu8; BLOCK];
        for i in 0..BLOCK {
            ipad[i] ^= k_block[i];
            opad[i] ^= k_block[i];
        }

        let mut inner = Sha256::new();
        inner.update(ipad);
        inner.update(msg);
        let inner_digest = inner.finalize();

        let mut outer = Sha256::new();
        outer.update(opad);
        outer.update(inner_digest);
        outer.finalize().into()
    }
}

/// Encode claims into a hex-encoded, fixed-layout payload string. Layout is
/// four fields joined by `:` then hex-encoded as a whole, so the `.`
/// token-field separator can never appear inside the payload.
fn encode_payload(claims: &TokenClaims) -> String {
    let raw = format!(
        "{}:{}:{}:{}",
        claims.session_id.as_uuid(),
        claims.agent_group_id.as_uuid(),
        claims.issued_at,
        claims.expires_at,
    );
    hex::encode(raw.as_bytes())
}

/// Decode a payload produced by [`encode_payload`]. Returns `None` on any
/// structural problem; callers only reach this after the HMAC verified, so a
/// `None` here means our own encoding drifted, not an attacker.
fn decode_payload(payload: &str) -> Option<TokenClaims> {
    let bytes = hex::decode(payload).ok()?;
    let s = String::from_utf8(bytes).ok()?;
    let mut fields = s.split(':');
    let session = fields.next()?;
    let group = fields.next()?;
    let issued = fields.next()?;
    let expires = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    Some(TokenClaims {
        session_id: SessionId(session.parse().ok()?),
        agent_group_id: AgentGroupId(group.parse().ok()?),
        issued_at: issued.parse().ok()?,
        expires_at: expires.parse().ok()?,
    })
}

/// Constant-time byte-slice equality. Returns `false` immediately on a length
/// mismatch (length is not secret), then ORs all byte differences so the loop
/// runs the full length regardless of where the first mismatch is.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The budget side of the authz decision. The broker enforces the group's
/// budget a *second* time at request time (the spawn gate already refuses to
/// spawn over-budget): a long-lived container could otherwise keep calling
/// the model across a budget rollover without a respawn. Computed by the
/// caller (which owns the DB) and handed to [`authorize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetVerdict {
    /// The group is within budget — forward the request.
    WithinBudget,
    /// The group is over budget — refuse the request at the broker.
    OverBudget,
}

/// The end-to-end authorization outcome for one model request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthzDecision {
    /// Validated and within budget: forward upstream with the real key. The
    /// claims are carried so the caller can attribute metering to the group.
    Forward(TokenClaims),
    /// Token failed validation. Maps to HTTP 401.
    Unauthorized(TokenError),
    /// Token valid but the group is over budget. Maps to HTTP 429.
    OverBudget(TokenClaims),
}

/// Decide whether to forward a request, given the presented token, the
/// current time, the revocation set, and a budget-check closure.
///
/// The budget closure is invoked **only after** the token validates, so an
/// unauthenticated caller can never trigger a DB read — and so the
/// over-budget outcome is always attributable to a real group. Pure and
/// fully unit-testable; the live listener calls this then acts on the result.
pub fn authorize(
    keyring: &BrokerKeyring,
    token: &str,
    now: u64,
    revocations: &Revocations,
    budget_check: impl FnOnce(&TokenClaims) -> BudgetVerdict,
) -> AuthzDecision {
    match keyring.validate(token, now, revocations) {
        Err(e) => AuthzDecision::Unauthorized(e),
        Ok(claims) => match budget_check(&claims) {
            BudgetVerdict::WithinBudget => AuthzDecision::Forward(claims),
            BudgetVerdict::OverBudget => AuthzDecision::OverBudget(claims),
        },
    }
}

/// A request the broker is about to forward upstream, modeled as a header set
/// so the auth-injection logic is testable without a live socket. The broker
/// receives the runner's request (whose auth header holds the *token*),
/// strips every client-supplied auth header, and injects the *real* key in
/// the slot the configured provider expects.
#[derive(Debug, Clone, Default)]
pub struct UpstreamRequest {
    /// Header name → value pairs, lowercased names. Auth headers here are the
    /// runner's (token-bearing) ones; [`Self::with_injected_auth`] replaces
    /// them.
    pub headers: Vec<(String, String)>,
}

/// Which header slot the upstream provider authenticates with. Anthropic uses
/// `x-api-key`; `OpenAI`-compatible bases (`OpenRouter` et al.) use
/// `Authorization: Bearer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    /// `x-api-key: <key>` (Anthropic).
    XApiKey,
    /// `Authorization: Bearer <key>` (OpenAI-compatible).
    AuthorizationBearer,
}

impl UpstreamRequest {
    /// Build from raw header pairs (names lowercased for case-insensitive
    /// matching).
    #[must_use]
    pub fn from_headers(headers: Vec<(String, String)>) -> Self {
        Self {
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v))
                .collect(),
        }
    }

    /// Produce a header set safe to send upstream: every client-supplied auth
    /// header (`x-api-key`, `authorization`) is dropped, then the real key is
    /// injected in the provider's expected slot. This is what guarantees the
    /// token the runner sent never reaches the upstream, and the real key is
    /// only ever added host-side here.
    #[must_use]
    pub fn with_injected_auth(&self, scheme: AuthScheme, real_key: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .headers
            .iter()
            .filter(|(k, _)| k != "x-api-key" && k != "authorization")
            .cloned()
            .collect();
        match scheme {
            AuthScheme::XApiKey => out.push(("x-api-key".to_string(), real_key.to_string())),
            AuthScheme::AuthorizationBearer => {
                out.push(("authorization".to_string(), format!("Bearer {real_key}")));
            }
        }
        out
    }
}

/// Resolve the auth scheme from the resolved provider string (same
/// normalisation the runner/egress resolver use). Anthropic-envelope
/// providers authenticate with `x-api-key`; `OpenAI`-compatible ones (including
/// `OpenRouter`) use `Authorization: Bearer`.
#[must_use]
pub fn auth_scheme_for_provider(provider: Option<&str>) -> AuthScheme {
    match provider {
        // OpenRouter speaks the Anthropic envelope but authenticates with a
        // Bearer token, so it is the notable Bearer case among the
        // anthropic-shaped providers.
        Some("openrouter") => AuthScheme::AuthorizationBearer,
        // anthropic / claude / ollama-shim / unknown default to x-api-key,
        // matching `AnthropicProvider`'s header.
        _ => AuthScheme::XApiKey,
    }
}

/// Static configuration for the broker, resolved once at boot. Holds the real
/// upstream key + base URL (which the broker forwards to) and the token TTL.
/// The signing keyring lives in [`BrokerState`].
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// The REAL provider key. Held only here, host-side; never written to a
    /// container env once the broker is enabled.
    pub upstream_key: String,
    /// The upstream base URL the broker forwards model calls to (the operator's
    /// `ANTHROPIC_BASE_URL`, or the Anthropic default when unset).
    pub upstream_base_url: String,
    /// Capability-token lifetime.
    pub token_ttl: Duration,
}

impl BrokerConfig {
    /// Resolve broker config from the process env. Returns `None` (broker
    /// disabled, default behaviour) unless `COPPERCLAW_CREDENTIAL_BROKER` is
    /// truthy AND a real upstream key is present — without a key there is
    /// nothing to broker, so we fall back to the unchanged default path with
    /// a warning rather than silently breaking model calls.
    ///
    /// `enabled`, `upstream_key`, `upstream_base_url`, and `ttl_override` are
    /// passed in (rather than read from the global env here) so the resolver
    /// is unit-testable and the caller controls precedence.
    #[must_use]
    pub fn resolve(
        enabled: bool,
        upstream_key: Option<&str>,
        upstream_base_url: Option<&str>,
        ttl_override_secs: Option<u64>,
    ) -> Option<Self> {
        if !enabled {
            return None;
        }
        let key = upstream_key.filter(|k| !k.is_empty())?;
        let base = upstream_base_url
            .filter(|b| !b.is_empty())
            .unwrap_or(super::egress::DEFAULT_ANTHROPIC_BASE_URL)
            .to_string();
        let ttl = ttl_override_secs
            .filter(|s| *s > 0)
            .unwrap_or(DEFAULT_TOKEN_TTL_SECS);
        Some(Self {
            upstream_key: key.to_string(),
            upstream_base_url: base,
            token_ttl: Duration::from_secs(ttl),
        })
    }

    /// Parse the operator-facing `COPPERCLAW_CREDENTIAL_BROKER` toggle. Truthy
    /// values (`1`/`true`/`yes`/`on`/`enable`) turn the broker on; everything
    /// else (including unset) keeps the default off — strictly opt-in.
    #[must_use]
    pub fn parse_enabled(raw: Option<&str>) -> bool {
        matches!(
            raw.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
            Some("1" | "true" | "yes" | "on" | "enable" | "enabled")
        )
    }
}

/// Live broker state: the static config plus the signing keyring and the
/// mutable revocation set. Shared (behind an `Arc`) between the spawn path
/// (which mints tokens) and the loopback listener (which validates them).
#[derive(Debug)]
pub struct BrokerState {
    pub config: BrokerConfig,
    pub keyring: BrokerKeyring,
    pub revocations: RwLock<Revocations>,
}

impl BrokerState {
    /// Build live state from config with a fresh random signing keyring.
    #[must_use]
    pub fn new(config: BrokerConfig) -> Self {
        Self {
            config,
            keyring: BrokerKeyring::generate(),
            revocations: RwLock::new(Revocations::new()),
        }
    }

    /// Mint a token for `(session, group)` valid for the configured TTL from
    /// `now` (unix epoch seconds). Used by the spawn path.
    #[must_use]
    pub fn mint_for(&self, session_id: SessionId, group: AgentGroupId, now: u64) -> String {
        self.keyring
            .mint(session_id, group, now, self.config.token_ttl)
    }

    /// Revoke a session's token (poison-tolerant lock).
    pub fn revoke_session(&self, session_id: SessionId) {
        self.revocations
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revoke_session(session_id);
    }
}

/// Current unix epoch seconds. A pre-1970 clock (impossible on a sane host)
/// saturates to 0, which only makes tokens look older — fail-safe.
#[must_use]
pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn fixed_keyring() -> BrokerKeyring {
        BrokerKeyring::from_secret([7u8; SIGNING_KEY_LEN])
    }

    fn ids() -> (SessionId, AgentGroupId) {
        (SessionId(Uuid::new_v4()), AgentGroupId(Uuid::new_v4()))
    }

    // ── token mint / validate ────────────────────────────────────────────

    #[test]
    fn mint_then_validate_roundtrips_claims() {
        let kr = fixed_keyring();
        let (sid, gid) = ids();
        let token = kr.mint(sid, gid, 1_000, Duration::from_secs(3600));
        let revs = Revocations::new();
        let claims = kr.validate(&token, 1_500, &revs).expect("valid");
        assert_eq!(claims.session_id, sid);
        assert_eq!(claims.agent_group_id, gid);
        assert_eq!(claims.issued_at, 1_000);
        assert_eq!(claims.expires_at, 4_600);
    }

    #[test]
    fn token_has_expected_prefix_and_three_fields() {
        let kr = fixed_keyring();
        let (sid, gid) = ids();
        let token = kr.mint(sid, gid, 0, Duration::from_secs(60));
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "token must be prefix.payload.sig");
        assert_eq!(parts[0], "cct1");
    }

    // ── expiry ───────────────────────────────────────────────────────────

    #[test]
    fn validate_rejects_expired_token() {
        let kr = fixed_keyring();
        let (sid, gid) = ids();
        let token = kr.mint(sid, gid, 1_000, Duration::from_secs(100));
        let revs = Revocations::new();
        // expires_at = 1100; now == 1100 is already expired (>= boundary).
        assert_eq!(kr.validate(&token, 1_100, &revs), Err(TokenError::Expired));
        assert_eq!(kr.validate(&token, 5_000, &revs), Err(TokenError::Expired));
        // One second before expiry is still valid.
        assert!(kr.validate(&token, 1_099, &revs).is_ok());
    }

    // ── signature / tampering ────────────────────────────────────────────

    #[test]
    fn validate_rejects_forged_signature() {
        let kr = fixed_keyring();
        let (sid, gid) = ids();
        let token = kr.mint(sid, gid, 0, Duration::from_secs(3600));
        // Flip the last hex char of the signature.
        let mut chars: Vec<char> = token.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        let revs = Revocations::new();
        assert_eq!(
            kr.validate(&tampered, 10, &revs),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn validate_rejects_tampered_payload() {
        let kr = fixed_keyring();
        let (sid, gid) = ids();
        let token = kr.mint(sid, gid, 0, Duration::from_secs(3600));
        let parts: Vec<&str> = token.split('.').collect();
        // Re-encode the payload with a different (attacker-chosen) group so
        // they could bill another group. Signature is over the old payload,
        // so this must fail.
        let evil = TokenClaims {
            session_id: sid,
            agent_group_id: AgentGroupId(Uuid::new_v4()),
            issued_at: 0,
            expires_at: 3_600,
        };
        let forged = format!("{}.{}.{}", parts[0], encode_payload(&evil), parts[2]);
        let revs = Revocations::new();
        assert_eq!(
            kr.validate(&forged, 10, &revs),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn validate_rejects_token_from_other_keyring() {
        let kr_a = BrokerKeyring::from_secret([1u8; SIGNING_KEY_LEN]);
        let kr_b = BrokerKeyring::from_secret([2u8; SIGNING_KEY_LEN]);
        let (sid, gid) = ids();
        let token = kr_a.mint(sid, gid, 0, Duration::from_secs(3600));
        let revs = Revocations::new();
        assert_eq!(
            kr_b.validate(&token, 10, &revs),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn validate_rejects_malformed_tokens() {
        let kr = fixed_keyring();
        let revs = Revocations::new();
        for bad in ["", "nope", "cct1.only", "cct1.a.b.c", "wrong.payload.sig"] {
            let outcome = kr.validate(bad, 10, &revs);
            assert!(outcome.is_err(), "{bad:?} should be rejected: {outcome:?}");
        }
    }

    #[test]
    fn generated_keyrings_have_distinct_secrets() {
        // Two freshly generated keyrings should not validate each other's
        // tokens — i.e. the random secret actually varies.
        let kr_a = BrokerKeyring::generate();
        let kr_b = BrokerKeyring::generate();
        let (sid, gid) = ids();
        let token = kr_a.mint(sid, gid, 0, Duration::from_secs(3600));
        let revs = Revocations::new();
        assert!(kr_a.validate(&token, 10, &revs).is_ok());
        assert_eq!(
            kr_b.validate(&token, 10, &revs),
            Err(TokenError::BadSignature)
        );
    }

    // ── revocation ───────────────────────────────────────────────────────

    #[test]
    fn validate_rejects_revoked_session() {
        let kr = fixed_keyring();
        let (sid, gid) = ids();
        let token = kr.mint(sid, gid, 0, Duration::from_secs(3600));
        let mut revs = Revocations::new();
        assert!(kr.validate(&token, 10, &revs).is_ok());
        revs.revoke_session(sid);
        assert_eq!(kr.validate(&token, 10, &revs), Err(TokenError::Revoked));
    }

    #[test]
    fn group_watermark_revokes_old_tokens_but_not_new_ones() {
        let kr = fixed_keyring();
        let (sid_old, gid) = ids();
        let sid_new = SessionId(Uuid::new_v4());
        let old = kr.mint(sid_old, gid, 100, Duration::from_secs(3600));
        let new = kr.mint(sid_new, gid, 300, Duration::from_secs(3600));
        let mut revs = Revocations::new();
        // Revoke everything for the group issued before t=200.
        revs.revoke_group_before(gid, 200);
        assert_eq!(
            kr.validate(&old, 500, &revs),
            Err(TokenError::Revoked),
            "issued_at=100 < watermark 200"
        );
        assert!(
            kr.validate(&new, 500, &revs).is_ok(),
            "issued_at=300 >= watermark 200"
        );
    }

    #[test]
    fn revoke_group_before_is_monotonic() {
        let mut revs = Revocations::new();
        let gid = AgentGroupId(Uuid::new_v4());
        revs.revoke_group_before(gid, 500);
        // A lower value must not lower the watermark.
        revs.revoke_group_before(gid, 100);
        let claims = TokenClaims {
            session_id: SessionId(Uuid::new_v4()),
            agent_group_id: gid,
            issued_at: 300,
            expires_at: u64::MAX,
        };
        assert!(revs.is_revoked(&claims), "watermark must stay at 500");
    }

    #[test]
    fn group_watermark_does_not_revoke_other_groups() {
        let kr = fixed_keyring();
        let (sid, gid) = ids();
        let other_group = AgentGroupId(Uuid::new_v4());
        let token = kr.mint(sid, gid, 100, Duration::from_secs(3600));
        let mut revs = Revocations::new();
        revs.revoke_group_before(other_group, 1_000);
        assert!(
            kr.validate(&token, 200, &revs).is_ok(),
            "watermark on a different group must not affect this one"
        );
    }

    // ── authorize (end-to-end decision) ──────────────────────────────────

    #[test]
    fn authorize_forwards_valid_within_budget() {
        let kr = fixed_keyring();
        let (sid, gid) = ids();
        let token = kr.mint(sid, gid, 0, Duration::from_secs(3600));
        let revs = Revocations::new();
        let decision = authorize(&kr, &token, 10, &revs, |_| BudgetVerdict::WithinBudget);
        match decision {
            AuthzDecision::Forward(c) => {
                assert_eq!(c.session_id, sid);
                assert_eq!(c.agent_group_id, gid);
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn authorize_refuses_over_budget_after_validating() {
        let kr = fixed_keyring();
        let (sid, gid) = ids();
        let token = kr.mint(sid, gid, 0, Duration::from_secs(3600));
        let revs = Revocations::new();
        let decision = authorize(&kr, &token, 10, &revs, |_| BudgetVerdict::OverBudget);
        assert!(matches!(decision, AuthzDecision::OverBudget(c) if c.agent_group_id == gid));
    }

    #[test]
    fn authorize_does_not_run_budget_check_for_bad_token() {
        let kr = fixed_keyring();
        let revs = Revocations::new();
        let mut called = false;
        let decision = authorize(&kr, "garbage", 10, &revs, |_| {
            called = true;
            BudgetVerdict::WithinBudget
        });
        assert!(matches!(decision, AuthzDecision::Unauthorized(_)));
        assert!(!called, "budget check must not run for an invalid token");
    }

    #[test]
    fn authorize_reports_unauthorized_for_expired() {
        let kr = fixed_keyring();
        let (sid, gid) = ids();
        let token = kr.mint(sid, gid, 0, Duration::from_secs(10));
        let revs = Revocations::new();
        let decision = authorize(&kr, &token, 100, &revs, |_| BudgetVerdict::WithinBudget);
        assert!(matches!(
            decision,
            AuthzDecision::Unauthorized(TokenError::Expired)
        ));
    }

    // ── header injection ─────────────────────────────────────────────────

    #[test]
    fn injects_x_api_key_and_strips_token_header() {
        let req = UpstreamRequest::from_headers(vec![
            ("X-Api-Key".into(), "cct1.token.sig".into()),
            ("anthropic-version".into(), "2023-06-01".into()),
            ("content-type".into(), "application/json".into()),
        ]);
        let out = req.with_injected_auth(AuthScheme::XApiKey, "sk-real-master");
        // The token header is gone; the real key is present exactly once.
        let api_keys: Vec<&String> = out
            .iter()
            .filter(|(k, _)| k == "x-api-key")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(api_keys, vec![&"sk-real-master".to_string()]);
        // Non-auth headers survive.
        assert!(
            out.iter()
                .any(|(k, v)| k == "anthropic-version" && v == "2023-06-01")
        );
        // The token value never appears anywhere in the outgoing headers.
        assert!(!out.iter().any(|(_, v)| v.contains("cct1.token.sig")));
    }

    #[test]
    fn injects_bearer_and_strips_client_authorization() {
        let req = UpstreamRequest::from_headers(vec![
            ("Authorization".into(), "Bearer cct1.token.sig".into()),
            ("content-type".into(), "application/json".into()),
        ]);
        let out = req.with_injected_auth(AuthScheme::AuthorizationBearer, "sk-or-real");
        let auths: Vec<&String> = out
            .iter()
            .filter(|(k, _)| k == "authorization")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(auths, vec![&"Bearer sk-or-real".to_string()]);
        assert!(!out.iter().any(|(_, v)| v.contains("cct1.token.sig")));
    }

    #[test]
    fn injection_strips_both_auth_header_kinds_regardless_of_scheme() {
        // A runner that sent both must not leak either upstream.
        let req = UpstreamRequest::from_headers(vec![
            ("x-api-key".into(), "token-a".into()),
            ("authorization".into(), "Bearer token-b".into()),
        ]);
        let out = req.with_injected_auth(AuthScheme::XApiKey, "real");
        assert!(!out.iter().any(|(_, v)| v.contains("token-a")));
        assert!(!out.iter().any(|(_, v)| v.contains("token-b")));
        assert_eq!(out.iter().filter(|(k, _)| k == "x-api-key").count(), 1);
        assert_eq!(out.iter().filter(|(k, _)| k == "authorization").count(), 0);
    }

    #[test]
    fn auth_scheme_resolution_matches_provider() {
        assert_eq!(
            auth_scheme_for_provider(Some("anthropic")),
            AuthScheme::XApiKey
        );
        assert_eq!(
            auth_scheme_for_provider(Some("claude")),
            AuthScheme::XApiKey
        );
        assert_eq!(auth_scheme_for_provider(None), AuthScheme::XApiKey);
        assert_eq!(
            auth_scheme_for_provider(Some("openrouter")),
            AuthScheme::AuthorizationBearer
        );
    }

    // ── config resolution ────────────────────────────────────────────────

    #[test]
    fn parse_enabled_is_opt_in() {
        assert!(!BrokerConfig::parse_enabled(None));
        assert!(!BrokerConfig::parse_enabled(Some("")));
        assert!(!BrokerConfig::parse_enabled(Some("0")));
        assert!(!BrokerConfig::parse_enabled(Some("off")));
        assert!(BrokerConfig::parse_enabled(Some("1")));
        assert!(BrokerConfig::parse_enabled(Some("true")));
        assert!(BrokerConfig::parse_enabled(Some("ON")));
        assert!(BrokerConfig::parse_enabled(Some("enable")));
    }

    #[test]
    fn resolve_returns_none_when_disabled() {
        assert!(BrokerConfig::resolve(false, Some("sk-x"), None, None).is_none());
    }

    #[test]
    fn resolve_returns_none_without_key() {
        assert!(BrokerConfig::resolve(true, None, None, None).is_none());
        assert!(BrokerConfig::resolve(true, Some(""), None, None).is_none());
    }

    #[test]
    fn resolve_defaults_base_url_and_ttl() {
        let cfg = BrokerConfig::resolve(true, Some("sk-x"), None, None).unwrap();
        assert_eq!(cfg.upstream_key, "sk-x");
        assert_eq!(
            cfg.upstream_base_url,
            super::super::egress::DEFAULT_ANTHROPIC_BASE_URL
        );
        assert_eq!(cfg.token_ttl, Duration::from_secs(DEFAULT_TOKEN_TTL_SECS));
    }

    #[test]
    fn resolve_honours_overrides() {
        let cfg = BrokerConfig::resolve(
            true,
            Some("sk-x"),
            Some("https://proxy.example/v1"),
            Some(120),
        )
        .unwrap();
        assert_eq!(cfg.upstream_base_url, "https://proxy.example/v1");
        assert_eq!(cfg.token_ttl, Duration::from_secs(120));
    }

    // ── BrokerState integration of mint + revoke ─────────────────────────

    #[test]
    fn broker_state_mint_validates_and_revocation_takes_effect() {
        let cfg = BrokerConfig::resolve(true, Some("sk-real"), None, Some(3600)).unwrap();
        let state = BrokerState::new(cfg);
        let (sid, gid) = ids();
        let token = state.mint_for(sid, gid, 1_000);

        let decision = {
            let revs = state.revocations.read().unwrap();
            authorize(&state.keyring, &token, 1_010, &revs, |_| {
                BudgetVerdict::WithinBudget
            })
        };
        assert!(matches!(decision, AuthzDecision::Forward(_)));

        state.revoke_session(sid);
        let decision = {
            let revs = state.revocations.read().unwrap();
            authorize(&state.keyring, &token, 1_010, &revs, |_| {
                BudgetVerdict::WithinBudget
            })
        };
        assert!(matches!(
            decision,
            AuthzDecision::Unauthorized(TokenError::Revoked)
        ));
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
