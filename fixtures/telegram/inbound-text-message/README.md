## telegram / inbound-text-message

A minimal Telegram private-message round-trip. The harness injects a
single `InboundEvent` with `channel_type=telegram`, `platform_id=100`
(the chat_id used in `copperclaw-channels-telegram`'s own ingress unit
tests). Claude responds with one plain-text turn; the runner emits one
outbound chat row; the telegram `MockAdapter` records one delivery.
