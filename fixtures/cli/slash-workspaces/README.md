# cli / slash-workspaces

Pins the multi-workspace UX (`/switch <name>` + `/projects`) on the cli
channel. Both commands are HOST-ANSWERED, like `/status`: the router
synthesizes the reply from the session's on-disk `/data` dir and writes it
straight to `messages_out`. No runner turn fires.

Two inbound steps against one shared session:

1. `/switch fairway-focus` —
   - creates `/data/fairway-focus` under the session root (new workspace),
   - overwrites `<session_root>/.shell_state` with `cd '/data/fairway-focus'`
     so the agent's next `shell` command runs there (the shell tool sources
     that file before every command),
   - enqueues a context-reset `/clear` passthrough row in `messages_in`
     (trigger, `content.command = "clear"`). The harness does not drive the
     runner for a host-answered command, so the row is left `pending` here;
     in production the container manager spawns the runner, which wipes
     history via its existing `/clear` sentinel (pinned by `cli/slash-clear`).
   - host-answers the operator with the switch confirmation (`messages_out`
     seq 1).
2. `/projects` — host-answers the workspace listing (`messages_out` seq 3),
   marking `fairway-focus` active (resolved from the `.shell_state` written
   in step 1). System dirs (`inbox`, `outbox`, …) are excluded.

The registered test (`cli_slash_workspaces_switch_then_projects`) also reads
`.shell_state` back off the session root to assert the exact `cd` line
`/switch` wrote — the closest an in-process harness can get to "the runner's
next shell command lands in the new workspace".

Hand-authored (host-answer contract path — no live recording applicable).
