# cli/question-expiry — M21 F2: expire `ask_user_question` out loud

Registered test: `replay.rs::cli_question_expiry_surfaces_lapse_and_resumes_on_reply`.

Pins the full ask -> expire -> late-reply -> resume sequence:

1. **Ask (fixture step 1).** The scripted turn calls `ask_user_question`
   ("Tabs or spaces?"). The runner's System row routes through a
   harness-installed `InteractiveModule` (the production delivery-action
   path, wired via `ReplayHarness::install_interactive_module`), so the
   question card reaches the cli `MockAdapter` and the pending question
   is recorded with its ask-time origin (session, agent group, ask-row
   id, channel routing).
2. **Expiry (test-side sweep pass).** A real `SweepService` with the
   SAME module handle (`set_question_store` — clones share state) runs
   one pass. It must surface the lapse exactly once: a terminal `edit`
   System row stamping the delivered card with the expiry note ("This
   question expired before anyone answered — just reply and I'll pick
   it up from there."), delivered through the adapter's typed edit API
   (no live buttons left behind), plus a `trigger = 0` synthetic
   `ask_user_question_result` (`status = "expired"`) row in the session
   inbox. A second sweep pass is byte-quiet.
3. **Late reply (fixture step 2).** The user's "spaces please" spawns a
   normal turn; the registered test asserts the provider request body
   carries the synthetic no-answer result (the agent's next turn sees
   it) and the reply is delivered normally.

Why the expiry is test-side, not manifest-driven: host-side sweep
timing runs on wall/tokio time, NOT the runner `TestClock`, so a
fixture cannot advance the 24h TTL (`fixtures/README-m21-wave1.md`,
reachability item 2). The test builds the module handle with a ZERO
TTL so the ask is already lapsed when the wall-clock sweep pass runs;
TTL *selection* precision is pinned by paused-time crate tests
(`copperclaw-host-sweep::service` and `copperclaw-modules::interactive`).
The fixture still owns every pipeline half: the streams above include
the sweep-written edit note (`messages_out` seq 9, keyed at the ask
row's seq 3) and the synthetic result row (`messages_in` seq 4).

`in_reply_to` is normalized to `<IRT>` by a manifest substitution: the
threading id on a streamed chat row depends on write timing inside the
turn (observed flipping between the inbound id and null across runs)
and is not what this fixture pins.

Regenerate expected streams:
`COPPERCLAW_M21F2_GENERATE=1 cargo test -p copperclaw-host --test replay cli_question_expiry -- --nocapture`
(then re-apply the `<IRT>` normalization to `messages-out.jsonl`).
