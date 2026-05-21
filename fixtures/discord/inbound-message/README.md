## discord / inbound-message

A user posts a message in a Discord guild channel (`channel_id=c1`).
The harness injects a single `InboundEvent` with `channel_type=discord`
and `platform_id="c1"`, mirroring the wire shape produced by
`ironclaw-channels-discord`'s `message_create_to_inbound` unit tests.
Claude responds with one plain-text turn; the runner emits one outbound
chat row; the discord `MockAdapter` records one delivery.
