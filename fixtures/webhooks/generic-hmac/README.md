## webhooks / generic-hmac

A generic HMAC-signed webhook hits the `webhooks` adapter's
`/hooks/grafana` endpoint (i.e. `platform_id="grafana"`), simulating a
Grafana/Stripe/Sentry-style alert payload. The harness injects a single
`InboundEvent` with `channel_type=webhooks`, `kind=webhook`, no sender.
Claude responds with a short ack; the runner emits one outbound chat
row; the webhooks `MockAdapter` records one delivery.
