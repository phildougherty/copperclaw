# Ollama provider audit

Team OLLAMA — May 2026.

This document captures the audit of `crates/ironclaw-providers/src/ollama.rs`
and the gaps fixed to make a vanilla `ollama serve` actually work
end-to-end with the runner's tool loops.

## TL;DR

The pre-existing implementation was a thin facade over `AnthropicProvider`
pointed at `<base_url>/v1/messages`. **Vanilla Ollama does not expose that
route** — it returns `404`. The shim only worked if you stood up a
LiteLLM-style proxy in front of Ollama that translated Anthropic Messages
to Ollama's native API. The README's "Ollama via the Anthropic shim" claim
described that proxy-fronted deployment, not stock Ollama.

This audit added a **native** code path against Ollama's `/api/chat`
NDJSON endpoint, kept the legacy shim mode reachable via
`OllamaProvider::shim(...)`, and made `OllamaProvider::new(...)` use the
native path by default.

## Gap matrix

Of the nine items in the audit charter, six were HIGH severity and have
been fixed. The remaining three were MEDIUM or already covered.

| # | Gap | Severity | Status |
|---|-----|----------|--------|
| 1 | Tool-use parsing (Ollama's native shape, not Anthropic's) | HIGH | Fixed — `pump_ndjson` parses `message.tool_calls[].function.{name, arguments}` and emits `ToolStart` + `ToolCall` + `ToolEnd` |
| 2 | Streaming (NDJSON, one frame per line) | HIGH | Fixed — line-buffered NDJSON pump replaces the SSE parser; each frame surfaces an `Activity` beat so the runner's liveness tracking still works |
| 3 | Abort / cancellation | MEDIUM | Already worked — abort drops the `JoinHandle` for the pump task and closes the `mpsc` channel, which propagates to the reqwest stream Drop |
| 4 | Usage reporting | HIGH | Fixed — `prompt_eval_count` / `eval_count` from the final NDJSON frame map onto `ProviderEvent::Usage`; previously always 0 |
| 5 | Model name translation | LOW | Already worked — model name passes through verbatim; default fallback applied only when `QueryInput::model` is empty |
| 6 | Tool schema translation | HIGH | Fixed — `tools_to_openai_form` emits the `{type:"function", function:{name, description, parameters}}` shape Ollama expects |
| 7 | Tool-result history translation | HIGH | Fixed — `HistoryMessage::Tool` renders as a `tool` role message with `tool_call_id`; `HistoryMessage::ToolUse` attaches as `tool_calls` on the preceding assistant turn |
| 8 | System prompt handling | MEDIUM | Fixed — system prompts now go in as a leading `system` role message (works with `/api/chat` whether or not tools are present) |
| 9 | Compaction | DERIVED | Works for free now that streaming + Result emission are correct; compaction is just another provider call |

## Modes

```rust
// Native: talks /api/chat NDJSON directly.
let p = OllamaProvider::new("http://localhost:11434", Some("llama3.1:8b".into()));

// Shim: defers to AnthropicProvider against a proxy in front of Ollama.
// Use this when you have e.g. LiteLLM or an ollama-anthropic-bridge.
let p = OllamaProvider::shim("http://localhost:8000", None);
```

`is_native()` reports which path a given instance uses.

## Wire-format notes

**Request body (`POST /api/chat`):**

```json
{
  "model": "llama3.1:8b",
  "stream": true,
  "messages": [
    { "role": "system", "content": "you are helpful" },
    { "role": "user",   "content": "weather in sf?" }
  ],
  "options": { "num_predict": 4096, "temperature": 0.3 },
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "weather",
        "description": "look up the weather",
        "parameters": { "type": "object", "properties": { "loc": { "type": "string" } } }
      }
    }
  ]
}
```

**Streaming response (NDJSON, one object per line):**

```
{"model":"...","message":{"role":"assistant","content":"hi"}}
{"model":"...","message":{"role":"assistant","content":" there"}}
{"model":"...","done":true,"done_reason":"stop","prompt_eval_count":12,"eval_count":3}
```

**Tool call mid-stream:**

```
{"model":"...","message":{"role":"assistant","content":"","tool_calls":[
  {"id":"call_1","function":{"name":"weather","arguments":{"loc":"sf"}}}
]}}
```

The provider reassembles consecutive `message.content` frames into a
single buffered string surfaced once at `done:true` via
`ProviderEvent::Result`. Per-frame text deltas are not exposed as a
separate event because `ProviderEvent` doesn't currently have a `TextDelta`
variant; that's a `// TODO(team-ollama)` for a later patch — the runner
consumes `Result` today.

## Wiring into the runner

`OllamaProvider` is not yet wired into `ironclaw-runner::main` — the
runner currently constructs an `AnthropicProvider` unconditionally
(`crates/ironclaw-runner/src/main.rs:70-74`). Wiring is out of scope for
this audit (runner-side change, separate file scope) but the provider
itself is now production-ready when that's plumbed up.

## Testing

* `tests/ollama_conformance.rs` — 12 wiremock-backed conformance tests
  covering every `ProviderEvent` emission path on the native code path.
* `tests/ollama_shim.rs` — pinning tests for the legacy Anthropic-shim
  path (renamed from `tests/ollama_sse.rs`).
* `tests/ollama_live.rs` — single `#[ignore]`d smoke test against a real
  Ollama server. Opt-in via `cargo test --ignored ollama_live -p ironclaw-providers`.

## Known limitations / follow-ups

* `// TODO(team-ollama)`: when `ProviderEvent::TextDelta` lands, the
  NDJSON pump can emit deltas instead of (or in addition to) the
  buffered `Result`. Today every frame surfaces `Activity` so liveness
  tracking still works, but the runner can't render token-by-token.
* `// TODO(team-ollama)`: the runner does not yet construct
  `OllamaProvider` from config; operators have to wire it themselves.
  Tracking ticket needed against `ironclaw-runner`.
* `// TODO(team-ollama)`: Ollama's `keep_alive` parameter is not yet
  surfaced. Long-lived local sessions could keep the model warm with
  e.g. `keep_alive: "30m"`.
