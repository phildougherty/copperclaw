## cli / empty-llm-response

Simulates the LLM returning empty turns — zero content blocks (no text,
no tool_use) — on every attempt. The runner retries the identical
request twice (three attempts total, `MAX_EMPTY_REPLY_ATTEMPTS`), so the
fixture scripts three empty turns; each attempt emits its own
usage_report system row. After the third empty the inbound is marked
failed and the empty-reply apology (noting the 3 consecutive attempts)
rides an Error-kind row to the channel. This fixture pins both the retry
count and the terminal apology so a regression in either direction — a
crash on empty content, or retries silently dropped — would surface.
