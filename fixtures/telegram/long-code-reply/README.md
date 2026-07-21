# telegram/long-code-reply

Pins the fence-aware chat-text splitter (M18/C2). Hand-authored (no
real recording available for this error-shaped path — the bug it
guards against is a split code fence rendering as garbage).

The Claude turn streams a single 4333-char text block: a 43-char
intro line, a blank line, then a ```python fence whose 25 code lines
(170 chars each, no spaces) make the fence alone 4288 chars — larger
than the telegram adapter's 4096-char `max_message_chars` cap. The
text is deliberately kept at 29 lines so it stays under the runner's
30-line long-output expander threshold (`EXPANDER_LINE_THRESHOLD`)
and actually reaches the delivery-loop splitter instead of the
collapsible path. The fixture asserts the splitter's full preference
ladder:

- Chunk 0 (43 chars): the natural cut falls inside the fence, but a
  pre-fence cut exists within the limit, so the intro is cut off
  BEFORE the fence and delivered alone.
- Chunk 1 (3604 chars): the fence itself outruns the cap, so the
  splitter cuts at a code-line boundary (no line torn in half) and
  closes the fence at the cut — the chunk ends with a bare ```.
- Chunk 2 (697 chars): the fence is reopened with the same info
  string (```python) and runs to the original closing fence.

Note the chunk sizes are governed by `markdown::effective_max`, not by
the raw 4096 cap: adapters render markdown to HTML *after* the split,
and escaping expands the text, so the splitter reserves
`RENDER_HEADROOM_PERCENT` (10%) and cuts at 3686. The boundary moved
from row 22 to row 20 when that headroom landed — chunk 1 was
previously 3946 chars, which is under 4096 but would have overrun the
cap once rendered. Balanced fences, the reopen info string, and total
line count are the invariants here; the exact cut row is not.

Every delivered chunk parses with balanced fences. The fixture also
asserts exactly one `messages_out` row (splitting is a delivery-layer
concern, not a runner concern) and three `delivered` entries with the
fixture's `platform_id`.

If the splitter regresses (cuts mid-fence without closing, drops the
reopen line's info string, or tears a code line), the per-chunk text
assertion surfaces as a `delivered/[n]/content/text` mismatch in the
diff report, and `crates/copperclaw-host/tests/replay.rs`'s
`telegram_long_code_reply_fence_balanced` fails its balance check.
