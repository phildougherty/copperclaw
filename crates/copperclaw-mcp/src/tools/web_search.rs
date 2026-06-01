//! `web_search`: keyword / semantic / agent-tuned web search.
//!
//! `web_fetch` already gives the agent a URL → body pipe, but it cannot
//! *find* URLs. This tool closes that gap by dispatching to one of four
//! supported search providers depending on what the operator has wired
//! up:
//!
//! | Provider | Env var | Strengths |
//! |---|---|---|
//! | [Tavily](https://docs.tavily.com/) | `TAVILY_API_KEY` | Agent-purpose-built; returns short, model-friendly snippets. Default when multiple keys are present. |
//! | [Exa](https://docs.exa.ai/) | `EXA_API_KEY` | Neural / semantic search; best for "find conceptually similar pages". |
//! | [Brave](https://brave.com/search/api/) | `BRAVE_SEARCH_API_KEY` | Independent web index; best for keyword-style lookups. |
//! | [SerpAPI](https://serpapi.com/) | `SERPAPI_API_KEY` | Wraps Google/Bing; broad coverage, higher cost. |
//!
//! Provider selection priority on each call:
//!
//! 1. Explicit `provider` argument in the tool call.
//! 2. `COPPERCLAW_WEB_SEARCH_PROVIDER` environment variable.
//! 3. Auto-detect from which API key is present in the container env,
//!    in the order `tavily, exa, brave, serpapi`.
//!
//! No keys present → `ToolError::Validation` with a message naming the
//! env vars the operator can set. Errors over silent fallback.
//!
//! Every provider response is normalised to the same JSON shape so the
//! model sees a stable schema regardless of which backend ran:
//!
//! ```json
//! {
//!   "query":     "...",
//!   "provider":  "tavily|exa|brave|serpapi",
//!   "results": [
//!     {
//!       "title":     "string",
//!       "url":       "https://...",
//!       "snippet":   "string",
//!       "published": "rfc3339 string (optional)",
//!       "score":     0.83  // optional, provider-specific
//!     }
//!   ],
//!   "elapsed_ms": 432
//! }
//! ```

use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{Duration, Instant};

/// Default cap on per-call result count.
///
/// Kept small because tool results live in conversation history forever
/// until compaction; the model should pivot to `web_fetch` for any result
/// that needs more depth than the snippet provides. Live failure mode
/// that motivated this: one `web_fetch` returned 344KB; four `web_search`
/// calls at the old 10-result cap would have added another ~35KB on top.
/// Callers can still raise this per-call via the `max_results` arg up to
/// [`MAX_RESULTS_CEILING`].
const DEFAULT_MAX_RESULTS: u32 = 5;
/// Ceiling on per-call result count (regardless of provider's own cap).
const MAX_RESULTS_CEILING: u32 = 25;
/// HTTP timeout per call. Search APIs typically respond in under 2s;
/// we leave headroom for the slowest provider (`SerpAPI` cold cache).
const DEFAULT_TIMEOUT_SECS: u64 = 15;
/// Cap on individual snippet length so a verbose provider doesn't blow
/// the model's context. Truncated on a UTF-8 boundary.
///
/// Kept small because tool results live in conversation history forever
/// until compaction; the model should pivot to `web_fetch` for any result
/// that needs more depth than the snippet provides. Live failure mode
/// that motivated this: one `web_fetch` returned 344KB; four `web_search`
/// calls at the old 4KB cap would have added another ~35KB on top. 400
/// bytes is enough to judge relevance and decide whether to fetch the
/// full page.
const SNIPPET_CAP_BYTES: usize = 400;

/// Default base URLs. Tests inject alternative URLs via the lower-
/// level `search_with_*` fns so wiremock can stand in for the real
/// service.
const DEFAULT_TAVILY_URL: &str = "https://api.tavily.com/search";
const DEFAULT_EXA_URL: &str = "https://api.exa.ai/search";
const DEFAULT_BRAVE_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const DEFAULT_SERPAPI_URL: &str = "https://serpapi.com/search.json";

/// All supported provider identifiers. The string form is what
/// callers pass in the tool's `provider` argument and what shows up
/// in the result envelope's `provider` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Tavily,
    Exa,
    Brave,
    SerpApi,
}

impl Provider {
    /// Stable string identifier used in the tool's `provider`
    /// argument and the output JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tavily => "tavily",
            Self::Exa => "exa",
            Self::Brave => "brave",
            Self::SerpApi => "serpapi",
        }
    }

    /// Parse the operator-facing string form. Accepts the same
    /// identifiers that `as_str` returns, case-insensitively, plus a
    /// couple of friendly aliases (`exa.ai`, `brave-search`).
    pub fn parse(s: &str) -> Result<Self, ToolError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "tavily" => Ok(Self::Tavily),
            "exa" | "exa.ai" => Ok(Self::Exa),
            "brave" | "brave-search" => Ok(Self::Brave),
            "serpapi" | "serp" => Ok(Self::SerpApi),
            other => Err(ToolError::Validation(format!(
                "unknown web_search provider `{other}`; expected one of \
                 tavily, exa, brave, serpapi"
            ))),
        }
    }

    /// Environment variable holding this provider's API key. The
    /// presence of a non-empty value here is what
    /// [`autodetect_provider`] looks for.
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Tavily => "TAVILY_API_KEY",
            Self::Exa => "EXA_API_KEY",
            Self::Brave => "BRAVE_SEARCH_API_KEY",
            Self::SerpApi => "SERPAPI_API_KEY",
        }
    }
}

/// Normalised search result. Provider-specific fields are dropped or
/// folded into `score` / `published` to keep the model-facing shape
/// stable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// Top-level output emitted as the tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchOutput {
    pub query: String,
    pub provider: &'static str,
    pub results: Vec<SearchResult>,
    pub elapsed_ms: u128,
}

/// Decoded tool input. Exposed as `pub` so the lower-level
/// [`search`] entry point can be called directly from tests without
/// going through the JSON-RPC parser.
#[derive(Debug, Clone, Deserialize)]
pub struct Input {
    pub query: String,
    #[serde(default)]
    pub max_results: Option<u32>,
    #[serde(default)]
    pub provider: Option<String>,
    /// Provider-specific search-type hint, e.g. `neural` / `keyword`
    /// / `auto` for Exa, or `news` / `general` for Tavily. Ignored by
    /// providers that don't support the value.
    #[serde(default)]
    pub search_type: Option<String>,
}

/// Lookup table for environment variables. Production uses
/// [`SystemEnv`]; tests build a [`MapEnv`].
pub trait EnvLookup: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
}

/// Production env lookup, backed by `std::env::var`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEnv;

impl EnvLookup for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }
}

/// In-memory env, used by tests.
#[derive(Debug, Default, Clone)]
pub struct MapEnv(pub std::collections::HashMap<String, String>);

impl MapEnv {
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl EnvLookup for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// Pick a provider given the call args and ambient environment.
///
/// Resolution order:
/// 1. The explicit `provider` arg if present.
/// 2. `COPPERCLAW_WEB_SEARCH_PROVIDER` env var if set.
/// 3. The first provider whose `env_var()` resolves to a non-empty
///    value, scanned in the order `tavily, exa, brave, serpapi` so
///    the agent-purpose-built option wins by default.
pub fn resolve_provider(
    explicit: Option<&str>,
    env: &dyn EnvLookup,
) -> Result<Provider, ToolError> {
    if let Some(name) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        let p = Provider::parse(name)?;
        ensure_key_present(p, env)?;
        return Ok(p);
    }
    if let Some(name) = env.get("COPPERCLAW_WEB_SEARCH_PROVIDER") {
        let p = Provider::parse(&name)?;
        ensure_key_present(p, env)?;
        return Ok(p);
    }
    autodetect_provider(env)
}

fn ensure_key_present(p: Provider, env: &dyn EnvLookup) -> Result<(), ToolError> {
    if env.get(p.env_var()).is_some() {
        Ok(())
    } else {
        Err(ToolError::Validation(format!(
            "web_search provider `{}` requires the `{}` environment variable, but it is unset",
            p.as_str(),
            p.env_var(),
        )))
    }
}

fn autodetect_provider(env: &dyn EnvLookup) -> Result<Provider, ToolError> {
    for p in [
        Provider::Tavily,
        Provider::Exa,
        Provider::Brave,
        Provider::SerpApi,
    ] {
        if env.get(p.env_var()).is_some() {
            return Ok(p);
        }
    }
    Err(ToolError::Validation(
        "web_search has no provider configured; set one of \
         TAVILY_API_KEY, EXA_API_KEY, BRAVE_SEARCH_API_KEY, SERPAPI_API_KEY \
         (or pass `provider` explicitly in the tool call)"
            .to_string(),
    ))
}

/// Truncate `s` to at most `cap` bytes on a UTF-8 boundary. Returns
/// the slice (no copy if it already fits).
fn cap_snippet(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Public entry point invoked by the [`ToolHandler`].
///
/// Visible at module scope so the runner's tool dispatch can call
/// directly without going through the rmcp shim — the rmcp wrapper
/// is the only real consumer in production, but the direct entry
/// also lets external tests skip the JSON-RPC layer.
pub async fn search(input: Input, env: &dyn EnvLookup) -> Result<SearchOutput, ToolError> {
    let query = input.query.trim().to_string();
    if query.is_empty() {
        return Err(ToolError::Validation("`query` must be non-empty".into()));
    }
    let max_results = input
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS_CEILING);
    let provider = resolve_provider(input.provider.as_deref(), env)?;
    let api_key = env.get(provider.env_var()).ok_or_else(|| {
        ToolError::Internal(format!(
            "api key for `{}` missing after resolve",
            provider.as_str()
        ))
    })?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .build()
        .map_err(|e| ToolError::Internal(format!("web_search client build: {e}")))?;

    let started = Instant::now();
    let results = match provider {
        Provider::Tavily => {
            tavily::search(
                &client,
                DEFAULT_TAVILY_URL,
                &api_key,
                &query,
                max_results,
                input.search_type.as_deref(),
            )
            .await?
        }
        Provider::Exa => {
            exa::search(
                &client,
                DEFAULT_EXA_URL,
                &api_key,
                &query,
                max_results,
                input.search_type.as_deref(),
            )
            .await?
        }
        Provider::Brave => {
            brave::search(&client, DEFAULT_BRAVE_URL, &api_key, &query, max_results).await?
        }
        Provider::SerpApi => {
            serpapi::search(
                &client,
                DEFAULT_SERPAPI_URL,
                &api_key,
                &query,
                max_results,
                input.search_type.as_deref(),
            )
            .await?
        }
    };
    Ok(SearchOutput {
        query,
        provider: provider.as_str(),
        results,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

pub fn schema() -> Tool {
    make_tool(
        "web_search",
        "Search the open web. Returns up to 25 normalised {title, url, snippet} entries. Provider auto-detected from configured API keys; override per-call with `provider: tavily|exa|brave|serpapi`.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["query"],
            "properties": {
                "query":       { "type": "string", "minLength": 1 },
                "max_results": { "type": ["integer", "null"], "minimum": 1, "maximum": 25 },
                "provider":    { "type": ["string", "null"], "enum": ["tavily", "exa", "brave", "serpapi", null] },
                "search_type": { "type": ["string", "null"] }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let env = SystemEnv;
    let output = search(input, &env).await?;
    Ok(success_json(&output))
}

struct Handler;

#[async_trait::async_trait]
impl ToolHandler for Handler {
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        handle(arguments, ctx).await
    }
}

pub fn entry() -> ToolEntry {
    ToolEntry {
        tool: schema(),
        handler: Box::new(Handler),
    }
}

// ── Provider implementations ──────────────────────────────────────────────

pub mod tavily {
    //! Tavily search — agent-tuned, returns short snippets.
    //!
    //! API reference: <https://docs.tavily.com/docs/rest-api/api-reference>.
    //! Request: POST JSON with `api_key`, `query`, `max_results`, optional
    //! `search_depth` (`basic` | `advanced`). Response: top-level
    //! `results: [{ title, url, content, score, published_date? }]`.

    use super::{SNIPPET_CAP_BYTES, SearchResult, cap_snippet, json};
    use crate::error::ToolError;

    pub async fn search(
        client: &reqwest::Client,
        url: &str,
        api_key: &str,
        query: &str,
        max_results: u32,
        search_type: Option<&str>,
    ) -> Result<Vec<SearchResult>, ToolError> {
        let mut body = json!({
            "api_key": api_key,
            "query": query,
            "max_results": max_results,
            "search_depth": "basic",
        });
        if let Some(st) = search_type {
            // Tavily exposes `search_depth` (basic|advanced) and
            // `topic` (general|news). We map the first sensible
            // interpretation here.
            match st {
                "basic" | "advanced" => {
                    body["search_depth"] = json!(st);
                }
                "news" | "general" | "finance" => {
                    body["topic"] = json!(st);
                }
                _ => {} // unknown hint: ignore rather than error
            }
        }
        let resp = client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Internal(format!("tavily request: {e}")))?;
        let status = resp.status();
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Internal(format!("tavily response: {e}")))?;
        if !status.is_success() {
            let msg = payload
                .get("detail")
                .or_else(|| payload.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("(no detail)")
                .to_string();
            return Err(ToolError::Internal(format!(
                "tavily returned {}: {}",
                status.as_u16(),
                msg
            )));
        }
        let arr = payload
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let out: Vec<SearchResult> = arr
            .into_iter()
            .map(|v| SearchResult {
                title: v
                    .get("title")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                url: v
                    .get("url")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                snippet: cap_snippet(
                    v.get("content").and_then(|x| x.as_str()).unwrap_or(""),
                    SNIPPET_CAP_BYTES,
                ),
                published: v
                    .get("published_date")
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
                score: v.get("score").and_then(serde_json::Value::as_f64),
            })
            .collect();
        Ok(out)
    }
}

pub mod exa {
    //! Exa search — neural / semantic web search.
    //!
    //! API reference: <https://docs.exa.ai/reference/search>. Request:
    //! POST JSON with `query`, `numResults`, `type` (`auto|neural|keyword`).
    //! Header `x-api-key`. Response: `results: [{ title, url, score,
    //! publishedDate?, text?, summary? }]`.
    //!
    //! We pass `contents: { text: true }` to opt in to snippet text;
    //! without it Exa returns metadata only.

    use super::{SNIPPET_CAP_BYTES, SearchResult, cap_snippet, json};
    use crate::error::ToolError;

    pub async fn search(
        client: &reqwest::Client,
        url: &str,
        api_key: &str,
        query: &str,
        max_results: u32,
        search_type: Option<&str>,
    ) -> Result<Vec<SearchResult>, ToolError> {
        let exa_type = match search_type.map_or("auto", str::trim) {
            "neural" | "keyword" | "auto" => search_type.unwrap_or("auto"),
            _ => "auto",
        };
        let body = json!({
            "query": query,
            "numResults": max_results,
            "type": exa_type,
            "contents": { "text": { "maxCharacters": SNIPPET_CAP_BYTES } },
        });
        let resp = client
            .post(url)
            .header("x-api-key", api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Internal(format!("exa request: {e}")))?;
        let status = resp.status();
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Internal(format!("exa response: {e}")))?;
        if !status.is_success() {
            let msg = payload
                .get("message")
                .or_else(|| payload.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("(no detail)")
                .to_string();
            return Err(ToolError::Internal(format!(
                "exa returned {}: {}",
                status.as_u16(),
                msg
            )));
        }
        let arr = payload
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let out = arr
            .into_iter()
            .map(|v| {
                let snippet_src = v
                    .get("text")
                    .or_else(|| v.get("summary"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                SearchResult {
                    title: v
                        .get("title")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    url: v
                        .get("url")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    snippet: cap_snippet(snippet_src, SNIPPET_CAP_BYTES),
                    published: v
                        .get("publishedDate")
                        .and_then(|x| x.as_str())
                        .map(str::to_string),
                    score: v.get("score").and_then(serde_json::Value::as_f64),
                }
            })
            .collect();
        Ok(out)
    }
}

pub mod brave {
    //! Brave search — independent index, keyword-tuned.
    //!
    //! API reference: <https://api.search.brave.com/app/documentation/web-search/get-started>.
    //! Request: GET with `q` query string and `count` for result count;
    //! header `X-Subscription-Token` for the key. Response:
    //! `web.results: [{ title, url, description, age?, published? }]`.

    use super::{SNIPPET_CAP_BYTES, SearchResult, cap_snippet};
    use crate::error::ToolError;

    pub async fn search(
        client: &reqwest::Client,
        url: &str,
        api_key: &str,
        query: &str,
        max_results: u32,
    ) -> Result<Vec<SearchResult>, ToolError> {
        let resp = client
            .get(url)
            .query(&[("q", query), ("count", &max_results.to_string())])
            .header("X-Subscription-Token", api_key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Internal(format!("brave request: {e}")))?;
        let status = resp.status();
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Internal(format!("brave response: {e}")))?;
        if !status.is_success() {
            let msg = payload
                .get("error")
                .and_then(|v| v.get("detail"))
                .or_else(|| payload.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("(no detail)")
                .to_string();
            return Err(ToolError::Internal(format!(
                "brave returned {}: {}",
                status.as_u16(),
                msg
            )));
        }
        let arr = payload
            .get("web")
            .and_then(|v| v.get("results"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let out = arr
            .into_iter()
            .map(|v| SearchResult {
                title: v
                    .get("title")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                url: v
                    .get("url")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                snippet: cap_snippet(
                    v.get("description").and_then(|x| x.as_str()).unwrap_or(""),
                    SNIPPET_CAP_BYTES,
                ),
                published: v
                    .get("page_age")
                    .or_else(|| v.get("age"))
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
                score: None,
            })
            .collect();
        Ok(out)
    }
}

pub mod serpapi {
    //! `SerpAPI` search — wraps Google/Bing/etc.
    //!
    //! API reference: <https://serpapi.com/search-api>. Request: GET with
    //! `api_key`, `q`, `num`, `engine` (default `google`). Response:
    //! `organic_results: [{ title, link, snippet, date?, position }]`.
    //!
    //! `search_type` selects the engine — pass `google`, `bing`,
    //! `duckduckgo`, etc. Default `google`.

    use super::{SNIPPET_CAP_BYTES, SearchResult, cap_snippet};
    use crate::error::ToolError;

    pub async fn search(
        client: &reqwest::Client,
        url: &str,
        api_key: &str,
        query: &str,
        max_results: u32,
        search_type: Option<&str>,
    ) -> Result<Vec<SearchResult>, ToolError> {
        let engine = match search_type.map_or("google", str::trim) {
            "" => "google",
            other => other,
        };
        // SerpAPI only authenticates via the `api_key` query string —
        // there is no header-based auth path. That means
        // `reqwest::Error::Display` will render the full request URL
        // (with the secret) on transport failures, and the message
        // ends up in tool output, persisted into agent history, and
        // re-sent upstream. We must build the error string ourselves
        // from the structured fields (`status()`, `is_timeout()`,
        // `is_connect()`) without ever invoking `Display` on the
        // underlying error.
        let resp = client
            .get(url)
            .query(&[
                ("api_key", api_key),
                ("q", query),
                ("num", &max_results.to_string()),
                ("engine", engine),
            ])
            .send()
            .await
            .map_err(|e| ToolError::Internal(redact_reqwest_error("serpapi request", &e)))?;
        let status = resp.status();
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Internal(redact_reqwest_error("serpapi response", &e)))?;
        if !status.is_success() {
            let msg = payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("(no detail)")
                .to_string();
            return Err(ToolError::Internal(format!(
                "serpapi returned {}: {}",
                status.as_u16(),
                msg
            )));
        }
        let arr = payload
            .get("organic_results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let out = arr
            .into_iter()
            .map(|v| SearchResult {
                title: v
                    .get("title")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                // SerpAPI's organic results carry the URL under `link`.
                url: v
                    .get("link")
                    .or_else(|| v.get("url"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                snippet: cap_snippet(
                    v.get("snippet").and_then(|x| x.as_str()).unwrap_or(""),
                    SNIPPET_CAP_BYTES,
                ),
                published: v.get("date").and_then(|x| x.as_str()).map(str::to_string),
                score: v
                    .get("position")
                    // Lower position = better; invert into a 0..1 score
                    // so the model can use it like other providers'
                    // scores. Position 1 ≈ 1.0, position 100 ≈ 0.01.
                    .and_then(serde_json::Value::as_f64)
                    .map(|p| (1.0_f64 / p.max(1.0_f64)).clamp(0.0, 1.0)),
            })
            .collect();
        Ok(out)
    }

    /// Build a `reqwest` error message that NEVER includes the
    /// request URL (which carries the `api_key` query parameter for
    /// `SerpAPI`). We only read structured fields so the rendered
    /// string is bounded by what we explicitly emit.
    ///
    /// IMPORTANT: do not add `e.to_string()` or `format!("{e}")` /
    /// `{:?}` here. `reqwest::Error`'s `Display` impl walks the
    /// request URL into the message; that's the leak we're closing.
    pub(super) fn redact_reqwest_error(prefix: &str, e: &reqwest::Error) -> String {
        let mut parts: Vec<String> = Vec::new();
        if e.is_timeout() {
            parts.push("timeout".into());
        }
        if e.is_connect() {
            parts.push("connect failure".into());
        }
        if e.is_request() {
            parts.push("request error".into());
        }
        if e.is_body() {
            parts.push("body error".into());
        }
        if e.is_decode() {
            parts.push("decode error".into());
        }
        if let Some(status) = e.status() {
            parts.push(format!("status {}", status.as_u16()));
        }
        if parts.is_empty() {
            // Unknown failure shape — emit a generic marker rather
            // than the upstream Display string (which would include
            // the URL).
            parts.push("transport error".into());
        }
        format!("{prefix}: {}", parts.join(", "))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn search_error_does_not_leak_api_key() {
            // Point at a port nothing is listening on so reqwest
            // returns a connect error. The api_key appears in the
            // request URL; the returned error string must NOT.
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(500))
                .build()
                .unwrap();
            let api_key = "SECRET_KEY_THAT_MUST_NEVER_LEAK";
            let err = search(
                &client,
                "http://127.0.0.1:1/search",
                api_key,
                "anything",
                3,
                None,
            )
            .await
            .expect_err("connect to port 1 should fail");
            let msg = format!("{err}");
            assert!(
                !msg.contains(api_key),
                "api key leaked into error message: {msg}"
            );
        }

        #[tokio::test]
        async fn search_error_against_bad_scheme_does_not_leak_key() {
            // Defence in depth: a different failure shape (invalid
            // URL → request build failure inside reqwest) must also
            // route through `redact_reqwest_error`. The url parse
            // itself happens inside reqwest::Client::get, surfacing
            // as a `reqwest::Error` of kind `Builder` or `Request`.
            let client = reqwest::Client::new();
            let api_key = "ANOTHER_SECRET_KEY_42";
            let err = search(&client, "not a url", api_key, "q", 1, None)
                .await
                .expect_err("invalid url must fail");
            let msg = format!("{err}");
            assert!(
                !msg.contains(api_key),
                "api key leaked into error message: {msg}"
            );
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn env_with(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv::from_pairs(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        )
    }

    // ---- Provider parsing / resolution ---------------------------------

    #[test]
    fn provider_parse_canonical_and_aliases() {
        assert_eq!(Provider::parse("tavily").unwrap(), Provider::Tavily);
        assert_eq!(Provider::parse("exa").unwrap(), Provider::Exa);
        assert_eq!(Provider::parse("exa.ai").unwrap(), Provider::Exa);
        assert_eq!(Provider::parse("brave").unwrap(), Provider::Brave);
        assert_eq!(Provider::parse("brave-search").unwrap(), Provider::Brave);
        assert_eq!(Provider::parse("serpapi").unwrap(), Provider::SerpApi);
        assert_eq!(Provider::parse("serp").unwrap(), Provider::SerpApi);
        // Case-insensitive.
        assert_eq!(Provider::parse("  BRAVE  ").unwrap(), Provider::Brave);
    }

    #[test]
    fn provider_parse_rejects_unknown() {
        let err = Provider::parse("yahoo").unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn resolve_provider_explicit_wins() {
        let env = env_with(&[
            ("TAVILY_API_KEY", "tav"),
            ("EXA_API_KEY", "exa"),
            ("COPPERCLAW_WEB_SEARCH_PROVIDER", "tavily"),
        ]);
        let p = resolve_provider(Some("exa"), &env).unwrap();
        assert_eq!(p, Provider::Exa);
    }

    #[test]
    fn resolve_provider_explicit_without_key_errors() {
        let env = env_with(&[("TAVILY_API_KEY", "tav")]);
        let err = resolve_provider(Some("exa"), &env).unwrap_err();
        match err {
            ToolError::Validation(m) => assert!(m.contains("EXA_API_KEY"), "{m}"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn resolve_provider_env_override_wins_over_autodetect() {
        let env = env_with(&[
            ("EXA_API_KEY", "exa"),
            ("BRAVE_SEARCH_API_KEY", "br"),
            ("COPPERCLAW_WEB_SEARCH_PROVIDER", "brave"),
        ]);
        let p = resolve_provider(None, &env).unwrap();
        assert_eq!(p, Provider::Brave);
    }

    #[test]
    fn resolve_provider_autodetect_order_tavily_first() {
        let env = env_with(&[
            ("TAVILY_API_KEY", "tav"),
            ("EXA_API_KEY", "exa"),
            ("BRAVE_SEARCH_API_KEY", "br"),
            ("SERPAPI_API_KEY", "serp"),
        ]);
        let p = resolve_provider(None, &env).unwrap();
        assert_eq!(p, Provider::Tavily);
    }

    #[test]
    fn resolve_provider_autodetect_falls_through() {
        // No tavily/exa, brave present.
        let env = env_with(&[("BRAVE_SEARCH_API_KEY", "br")]);
        let p = resolve_provider(None, &env).unwrap();
        assert_eq!(p, Provider::Brave);
    }

    #[test]
    fn resolve_provider_no_keys_errors() {
        let env = env_with(&[]);
        let err = resolve_provider(None, &env).unwrap_err();
        match err {
            ToolError::Validation(m) => {
                assert!(m.contains("TAVILY_API_KEY"));
                assert!(m.contains("EXA_API_KEY"));
                assert!(m.contains("BRAVE_SEARCH_API_KEY"));
                assert!(m.contains("SERPAPI_API_KEY"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- Helpers --------------------------------------------------------

    #[test]
    fn cap_snippet_below_limit_passthrough() {
        assert_eq!(cap_snippet("hello", 100), "hello");
    }

    #[test]
    fn cap_snippet_truncates_with_ellipsis() {
        let s = "x".repeat(100);
        let out = cap_snippet(&s, 10);
        assert!(out.ends_with('…'));
        assert!(out.chars().count() <= 11); // 10 chars + the ellipsis
    }

    #[test]
    fn cap_snippet_respects_char_boundary() {
        // Multibyte chars (emoji are >1 byte but inside cap).
        let s = "héllo wörld this is some text".to_string();
        let _ = cap_snippet(&s, 5); // Just ensure no panic.
    }

    // ---- Tavily provider ------------------------------------------------

    #[tokio::test]
    async fn tavily_happy_path_parses_results() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {
                        "title": "Rust language",
                        "url": "https://www.rust-lang.org",
                        "content": "Rust is a systems programming language.",
                        "score": 0.92,
                        "published_date": "2025-01-15T00:00:00Z"
                    },
                    {
                        "title": "No score result",
                        "url": "https://example.com",
                        "content": "missing score and date"
                    }
                ]
            })))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let results = tavily::search(&client, &format!("{}/", server.uri()), "k", "rust", 5, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust language");
        assert_eq!(results[0].url, "https://www.rust-lang.org");
        assert_eq!(
            results[0].snippet,
            "Rust is a systems programming language."
        );
        assert_eq!(results[0].score, Some(0.92));
        assert_eq!(
            results[0].published.as_deref(),
            Some("2025-01-15T00:00:00Z")
        );
        assert_eq!(results[1].score, None);
        assert_eq!(results[1].published, None);
    }

    #[tokio::test]
    async fn tavily_error_status_surfaces_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"detail": "invalid api key"})),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let err = tavily::search(&client, &format!("{}/", server.uri()), "k", "q", 5, None)
            .await
            .unwrap_err();
        match err {
            ToolError::Internal(m) => {
                assert!(m.contains("401"));
                assert!(m.contains("invalid api key"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tavily_empty_results_field_returns_empty_vec() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let out = tavily::search(&client, &format!("{}/", server.uri()), "k", "q", 5, None)
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    // ---- Exa provider --------------------------------------------------

    #[tokio::test]
    async fn exa_happy_path_parses_text_or_summary() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {
                        "title": "Page with text",
                        "url": "https://a.example",
                        "text": "long body content here",
                        "score": 0.81,
                        "publishedDate": "2024-12-01"
                    },
                    {
                        "title": "Page with only summary",
                        "url": "https://b.example",
                        "summary": "short summary"
                    }
                ]
            })))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let r = exa::search(&client, &format!("{}/", server.uri()), "k", "q", 5, None)
            .await
            .unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].snippet, "long body content here");
        assert_eq!(r[0].published.as_deref(), Some("2024-12-01"));
        assert_eq!(r[0].score, Some(0.81));
        assert_eq!(r[1].snippet, "short summary");
    }

    #[tokio::test]
    async fn exa_error_status_surfaces_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(json!({"message": "quota exceeded"})),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let err = exa::search(&client, &format!("{}/", server.uri()), "k", "q", 5, None)
            .await
            .unwrap_err();
        match err {
            ToolError::Internal(m) => {
                assert!(m.contains("403"));
                assert!(m.contains("quota exceeded"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- Brave provider ------------------------------------------------

    #[tokio::test]
    async fn brave_happy_path_parses_web_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("q", "rust"))
            .and(query_param("count", "5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "web": {
                    "results": [
                        {
                            "title": "Rust home",
                            "url": "https://rust-lang.org",
                            "description": "A language empowering everyone...",
                            "page_age": "2025-01-15T00:00:00"
                        }
                    ]
                }
            })))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let r = brave::search(&client, &format!("{}/", server.uri()), "k", "rust", 5)
            .await
            .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "Rust home");
        assert_eq!(r[0].url, "https://rust-lang.org");
        assert_eq!(r[0].snippet, "A language empowering everyone...");
        assert!(r[0].published.is_some());
        assert!(r[0].score.is_none()); // Brave doesn't expose scores.
    }

    #[tokio::test]
    async fn brave_missing_web_section_returns_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let r = brave::search(&client, &format!("{}/", server.uri()), "k", "q", 5)
            .await
            .unwrap();
        assert!(r.is_empty());
    }

    // ---- SerpAPI provider ---------------------------------------------

    #[tokio::test]
    async fn serpapi_happy_path_parses_organic_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("engine", "google"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "organic_results": [
                    {
                        "position": 1,
                        "title": "Top hit",
                        "link": "https://top.example",
                        "snippet": "First result",
                        "date": "Jan 15, 2025"
                    },
                    {
                        "position": 5,
                        "title": "Lower hit",
                        "link": "https://lower.example",
                        "snippet": "Fifth result"
                    }
                ]
            })))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let r = serpapi::search(&client, &format!("{}/", server.uri()), "k", "q", 5, None)
            .await
            .unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Top hit");
        assert_eq!(r[0].url, "https://top.example");
        assert!(r[0].score.unwrap() > r[1].score.unwrap());
        assert_eq!(r[0].published.as_deref(), Some("Jan 15, 2025"));
    }

    #[tokio::test]
    async fn serpapi_custom_engine() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("engine", "bing"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"organic_results": []})))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let r = serpapi::search(
            &client,
            &format!("{}/", server.uri()),
            "k",
            "q",
            5,
            Some("bing"),
        )
        .await
        .unwrap();
        assert!(r.is_empty()); // Test passes if the wiremock engine matcher matched.
    }

    // ---- search() top-level dispatch -----------------------------------

    #[tokio::test]
    async fn search_validation_empty_query() {
        let env = env_with(&[("TAVILY_API_KEY", "k")]);
        let err = search(
            Input {
                query: "  ".into(),
                max_results: None,
                provider: None,
                search_type: None,
            },
            &env,
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Validation(m) => assert!(m.contains("query")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_validation_no_keys() {
        let env = env_with(&[]);
        let err = search(
            Input {
                query: "anything".into(),
                max_results: None,
                provider: None,
                search_type: None,
            },
            &env,
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Validation(m) => {
                assert!(m.contains("TAVILY_API_KEY"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- Schema --------------------------------------------------------

    #[test]
    fn schema_is_well_formed() {
        let tool = schema();
        assert_eq!(tool.name, "web_search");
        let desc = tool.description.as_deref().unwrap_or("");
        assert!(
            desc.to_ascii_lowercase().contains("search"),
            "description must mention search: {desc:?}"
        );
        let schema_json: serde_json::Map<_, _> = (*tool.input_schema).clone();
        assert_eq!(schema_json["type"], json!("object"));
        let required = schema_json["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == &json!("query")));
    }

    #[test]
    fn provider_as_str_matches_parse() {
        for p in [
            Provider::Tavily,
            Provider::Exa,
            Provider::Brave,
            Provider::SerpApi,
        ] {
            assert_eq!(Provider::parse(p.as_str()).unwrap(), p);
        }
    }

    #[test]
    fn provider_env_var_distinct() {
        let names: Vec<&str> = [
            Provider::Tavily.env_var(),
            Provider::Exa.env_var(),
            Provider::Brave.env_var(),
            Provider::SerpApi.env_var(),
        ]
        .into_iter()
        .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }
}
