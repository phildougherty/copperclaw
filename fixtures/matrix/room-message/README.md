## matrix / room-message

A user posts an `m.room.message` (`msgtype=m.text`) in room
`!a:m.org`. The harness injects a single `InboundEvent` with
`channel_type=matrix` and `platform_id="!a:m.org"`, matching the shape
`ironclaw-channels-matrix`'s `event_to_inbound` emits. Claude responds
with one plain-text turn; the runner emits one outbound chat row; the
matrix `MockAdapter` records one delivery.
