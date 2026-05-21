## cli / multi-turn

Two-inbound, two-turn CLI replay. Inbound 001 says `ping`; the runner
serves Claude turn 001 (`pong`); the delivery loop dispatches the reply.
Inbound 002 then says `what was my last message?`; turn 002 replies
`your last message was 'ping'`. Both inbounds share a session
(`session_mode = shared`), so this exercises the runner threading
conversation state across multiple host-driven turns.
