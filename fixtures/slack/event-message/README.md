## slack / event-message

A minimal Slack Events-API channel-message round-trip. The harness
injects one `InboundEvent` with `channel_type=slack`, `platform_id=C1`
(the channel id used in `ironclaw-channels-slack`'s events-router unit
tests). Claude responds with one plain-text turn; the runner emits one
outbound chat row; the slack `MockAdapter` records one delivery.
