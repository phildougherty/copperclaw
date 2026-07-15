# telegram / slash-status

The telegram twin of `cli/slash-status`, in a mention-gated GROUP chat
(engage mode `mention`, no mention on the inbound): the fixture routes
only because recognised slash commands bypass the gate.

Asserts the host-answer contract: no `messages_in` row, no runner turn;
the router synthesizes the status reply from central-DB state and
writes it to `messages_out` with explicit telegram routing, and the
delivery loop hands it to the adapter (one `delivered` entry).

Hand-authored (host-answer contract path — no live recording
applicable).
