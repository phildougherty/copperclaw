# Webhooks TLS termination

Ironclaw webhook channels receive inbound events over HTTP. This document
explains why the host binds plain HTTP by default and how to add TLS in
production.

## Why plain HTTP on 127.0.0.1

Every webhook channel binds `127.0.0.1` by default, not `0.0.0.0`. This
is a direct consequence of the project's "secure-by-default,
public-by-deliberate-act" tenet: a channel is not reachable from the
network until an operator explicitly exposes it. Routing TLS termination
to a reverse proxy (rather than embedding rustls in every channel) keeps
each channel's authentication surface small and auditable. A reverse proxy
also lets you rotate certificates, adjust cipher suites, and add rate
limiting without touching the ironclaw binary.

Native rustls is **not** supported in 0.1.0 by deliberate design. Adding
TLS to every webhook channel would multiply the auth surface and introduce
per-channel certificate management complexity. The reverse-proxy approach
is the recommended and supported path.

## Supported deployment patterns

### Pattern 1 — Caddy reverse proxy

Caddy obtains and renews certificates automatically via ACME. The
`Caddyfile` snippet below terminates TLS on port 443 and forwards to the
ironclaw telegram webhook listener on port 8081.

```
# /etc/caddy/Caddyfile (or /home/<user>/.config/caddy/Caddyfile)

bot.example.com {
    # Caddy handles TLS automatically (Let's Encrypt by default).
    reverse_proxy /telegram/webhook 127.0.0.1:8081
    reverse_proxy /slack/events    127.0.0.1:8082
    # Add one reverse_proxy directive per channel.
}
```

After editing, reload Caddy:

```
caddy reload --config /etc/caddy/Caddyfile
```

Register each webhook URL with the upstream service using the HTTPS form:
`https://bot.example.com/telegram/webhook`.

### Pattern 2 — nginx reverse proxy

```nginx
# /etc/nginx/sites-available/ironclaw

server {
    listen 443 ssl http2;
    server_name bot.example.com;

    ssl_certificate     /etc/letsencrypt/live/bot.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/bot.example.com/privkey.pem;
    ssl_protocols       TLSv1.2 TLSv1.3;
    ssl_ciphers         HIGH:!aNULL:!MD5;

    location /telegram/webhook {
        proxy_pass http://127.0.0.1:8081;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    location /slack/events {
        proxy_pass http://127.0.0.1:8082;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    # Add one location block per channel.
}

server {
    listen 80;
    server_name bot.example.com;
    return 301 https://$host$request_uri;
}
```

Obtain a certificate with certbot before enabling the site:

```
certbot certonly --nginx -d bot.example.com
nginx -t && systemctl reload nginx
```

### Pattern 3 — Cloudflare Tunnel

Cloudflare Tunnel creates an outbound-only encrypted connection from your
server to Cloudflare's edge. No inbound firewall ports need to be opened
and no certificate management is required on your host.

Install `cloudflared` (see the Cloudflare documentation for the current
package), then create and configure a tunnel:

```bash
# Authenticate and create the tunnel.
cloudflared tunnel login
cloudflared tunnel create ironclaw

# Write the ingress config.
# Replace <TUNNEL-ID> with the UUID printed by the create command.
```

`~/.cloudflared/config.yml`:

```yaml
tunnel: <TUNNEL-ID>
credentials-file: /home/<user>/.cloudflared/<TUNNEL-ID>.json

ingress:
  - hostname: bot.example.com
    path: /telegram/webhook
    service: http://127.0.0.1:8081
  - hostname: bot.example.com
    path: /slack/events
    service: http://127.0.0.1:8082
  # Add one entry per channel.
  - service: http_status:404
```

Run the tunnel (or install the systemd service):

```bash
cloudflared tunnel run ironclaw
# or: cloudflared service install && systemctl start cloudflared
```

Create a DNS CNAME in the Cloudflare dashboard pointing
`bot.example.com` to `<TUNNEL-ID>.cfargotunnel.com`.

## Binding the channel listener on 0.0.0.0

When deploying with a reverse proxy that runs on a different host (or
inside a container), the channel needs to listen on all interfaces. Every
webhook channel exposes a `host` field in its per-channel config block:

```json
{
  "telegram": {
    "token": "...",
    "webhook": {
      "host": "0.0.0.0",
      "port": 8081,
      "path": "/telegram/webhook"
    }
  }
}
```

Set `IRONCLAW_CHANNELS_CONFIG` to a JSON object containing this override,
or write it directly to the channel's section if you use a structured
config file. The `host` field accepts any IP address or `0.0.0.0`.
**Only change this when the host is behind a network-level firewall or
a reverse proxy that validates the upstream connection. Exposing port
8081 directly on a public IP without TLS or signature verification is
insecure.**

Most webhook channels perform HMAC signature verification on every
inbound request (slack, github, linear, webex, whatsapp-cloud, line,
gchat, telegram, and the generic `webhooks` channel). A few use
shared-secret models instead: **teams** uses a constant-time
`clientState` compare, **mattermost** uses a `webhook_token` shared
secret on the query string / header. Verify your channel's specific
scheme — and that it's enabled — before binding on `0.0.0.0`.

## Per-channel default ports and paths

These are the constants in each `crates/ironclaw-channels/<name>/src/config.rs`
as of the current tree. **Note that telegram and slack default to
`0.0.0.0`** (they're typically fronted by a public proxy); every other
HTTP-listening channel defaults to `127.0.0.1` for a reverse-proxy
deployment.

| Channel | Default host | Default port | Default path |
|---------|-------------|-------------|--------------|
| telegram (webhook mode) | `0.0.0.0` | `8081` | `/telegram` |
| slack | `0.0.0.0` | `8082` | `/slack/events` |
| github | `127.0.0.1` | `8082` | `/github/webhook` |
| linear | `127.0.0.1` | `8083` | `/linear/webhook` |
| webex | `127.0.0.1` | `8084` | `/webex/webhook` |
| teams | `127.0.0.1` | `8085` | `/teams/webhook` |
| gchat | `127.0.0.1` | `8086` | `/gchat/webhook` |
| whatsapp-cloud | `127.0.0.1` | `8087` | `/whatsapp-cloud/webhook` |
| line | dynamic (port 0, OS-assigned) | dynamic | `/line/webhook` |
| mattermost | dynamic | dynamic | `/mattermost/webhook` |
| matrix | n/a (host issues `/sync` long-poll against Synapse) | n/a | n/a |
| webhooks (generic) | dynamic | dynamic | `/webhooks` |

Ports are stable defaults — override `port` in the channel config when
your reverse-proxy rule needs something else. Telegram + slack
defaulting to `0.0.0.0` is a property the deployment-time
config should override (or accept) consciously.

## Summary

1. Run ironclaw with channel listeners on `127.0.0.1` (the default).
2. Place a reverse proxy (Caddy, nginx, or Cloudflare Tunnel) in front.
3. Register the HTTPS URL (`https://bot.example.com/channel/path`) with
   the upstream service.
4. Only bind on `0.0.0.0` when the proxy is on a separate host and you
   have a firewall or signature verification in place.
