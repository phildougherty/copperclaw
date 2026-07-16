# gchat/approval-card — M19 U4 native card on Google Chat

**Card:** M19 U4 (native cards on gchat + matrix) — X-rider Wave 2 (parity
fixtures).

**What it proves.** Before U4, neither `gchat` nor `matrix` overrode the
trait `deliver_card` (`grep -c = 0`): gchat rendered cards only through its
internal `dispatch_action` path and matrix degraded cards to text — so the
M18 approval / ritual cards rendered as plain prose on both. U4 overrode
the trait `deliver_card` on both (gchat via its Cards-v2 builder, matrix
via `formatted_body` HTML with buttons-as-links). This fixture locks the
*pipeline* consequence on gchat: the runner's `send_card` reaches the gchat
adapter's `deliver_card` as a **structured card**, not a host-flattened
prose blob.

**Shape.** One gchat message ("is the deploy ready to ship?") drives a
two-turn scripted provider: turn 1 calls `send_card` with an approval card
(title, body, a `Change` field, an `Approve` **callback** button + a
`View diff` **url** button); turn 2 is the closing text answer. The runner
emits a `MessageKind::Card` row; the host delivery loop routes it to the
gchat adapter's `deliver_card`.

**Harness modelling.** The replay harness substitutes a `MockAdapter` for
the real gchat adapter. The bare mock's `deliver_card` degrades to the
trait-default text fallback (buttons become `- [Label] -> url` prose). The
manifest's `model_rich_cards: true` makes the harness's `CappedAdapter`
model gchat's card-capable contract: `deliver_card` records a
`MessageKind::Card`-kind delivery whose `content.card` preserves the full
structured card — so `snapshot_delivered` shows a native card on the wire
with both buttons intact, not flattened prose. This mirrors the F1
`matrix/hud-live-edit` fixture's `model_rich_breadcrumbs` mechanism, one
surface over (cards instead of breadcrumbs).

**Assertions** (in `tests/replay.rs`, on top of the byte-stable JSONL
diff): exactly one `card`-kind delivery to the originating gchat space; its
`content.card` carries the title, body, the field, and **both** buttons as
structured elements (the callback `value` and the `url` preserved, not
melted into a `Buttons:` prose block); and the model's closing text answer
is still delivered as its own chat message.

**What it does NOT exercise.** The *real* gchat Cards-v2 JSON the adapter
emits on the wire (or matrix's HTML `formatted_body` with buttons-as-links)
— those live in the respective adapter crates' own unit tests. This
fixture owns the host-side pipeline leg: a `send_card` is delivered to a
card-capable channel's `deliver_card` with its structure intact rather than
degraded host-side. It represents the U4 card surface at the pipeline level
for both gchat and matrix (the mock modelling is channel-agnostic; the
per-adapter native wire differences are the adapter unit tests' job).

Hand-authored (scripted `send_card` turn). Regenerate `expected/*.jsonl`
from a real run with `COPPERCLAW_XR2_GENERATE=1 cargo test -p
copperclaw-host --test replay gchat_approval_card`.
