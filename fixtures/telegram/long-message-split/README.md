# telegram/long-message-split

Pins the host-side chat-text splitter shipped in slice 1 of the
cohesive cross-channel UX baseline (see `CHANGELOG.md` [Unreleased]).

The Claude turn streams a single text block of 5002 chars,
which exceeds the `telegram` adapter's 4096-char `max_message_chars`
cap. The splitter cuts on the paragraph (`\n\n`) boundary, yielding
two chunks of 2500 chars each. The fixture asserts:

- Exactly one row in `messages_out` (the runner emitted one chat row;
  splitting is a delivery-layer concern, not a runner concern).
- Exactly two `delivered` entries — one per chunk — both with the
  fixture's `platform_id`.
- The recorded inbound `delivered` row references the FIRST chunk's
  platform-side id (visible only indirectly via the fact that
  delivery completed without re-queuing the row).

If the splitter regresses (e.g. cuts on the wrong boundary, drops a
trailing newline, or fails to detect the cap), the per-chunk text
assertion will surface as a `delivered/[0]/content/text` mismatch in
the diff report.
