# Push relay for Microsoft Graph change notifications

Design for a small server that turns Microsoft Graph "change notifications"
into a push channel for desktop clients (first client: penguin-ms-suite).
Written so it can seed a separate project.

Status: design only. Nothing here is built. Facts about Graph limits and
permissions are from memory of Microsoft's docs and MUST be re-checked
against current documentation before implementation (marked [verify]).

## 1. Why

The desktop app polls Graph (2 s open chat, 5 s focused list, 10 s
background). That works but costs latency and steady request traffic.
Graph can instead POST to a URL when a chat message changes. A laptop has
no public HTTPS address, so a relay sits in between:

    Microsoft Graph --HTTPS POST--> relay --WebSocket--> desktop client

The relay forwards only ids. The client then fetches the message with its
own token. Message content never touches the relay.

## 2. Components

    +-----------+   subscribe (client's token)   +---------+
    |  Desktop  |------------------------------->|  Graph  |
    |  client   |<-------+                       +----+----+
    +-----+-----+        | WebSocket                  | POST /hook/{deviceId}
          |              |  (ids only)                v
          | register   +-+--------+  validation   +-------+
          +----------->|  Relay   |<--------------| Graph |
                       +----------+               +-------+

- **Client**: creates and renews the Graph subscription, holds the WebSocket,
  falls back to polling when the relay is unhealthy.
- **Relay**: stateless-ish HTTPS endpoint plus WebSocket hub. In-memory map
  `deviceId -> websocket`. No database needed for v1.
- **Graph**: sends notifications to the relay's public URL.

### Token handling: pick option B

- **A. Relay holds user refresh tokens** and manages subscriptions. Works
  while clients are offline, but the relay then stores sensitive tokens.
- **B. Client creates and renews the subscription** with its own access
  token, setting `notificationUrl` to the relay. The relay holds no Graph
  tokens at all. If the client is offline the subscription lapses, which is
  fine because the client does a full refresh on reconnect. **Recommended.**

## 3. Graph subscription details [verify all]

- Resource: `/me/chats/getAllMessages` (delegated). Narrower alternative:
  `/chats/{id}/messages` per chat (one subscription per chat, more to renew).
- Permission: `Chat.Read` or `Chat.ReadWrite` (delegated). The penguin app
  already has `Chat.ReadWrite`.
- `changeType`: `created,updated`.
- `includeResourceData: false`. The notification then carries only resource
  ids. This avoids the encryption-certificate requirement that comes with
  resource data, and keeps content off the relay.
- Max subscription lifetime for chat messages is short (about 60 minutes).
  Renew with `PATCH /subscriptions/{id}` at roughly half the lifetime.
- `clientState`: random secret per subscription. The relay checks it on every
  notification and drops mismatches.
- `lifecycleNotificationUrl`: same relay, separate path. Handles
  `reauthorizationRequired`, `subscriptionRemoved`, `missed`.
- Pricing: ordinary delegated change notifications are, to my knowledge, not
  metered. Teams APIs used with application permissions can be metered
  (payment models A/B). Confirm for the tenant.

### Webhook handshake and response rules

- On subscription create, Graph sends `POST /hook/{deviceId}?validationToken=X`.
  Relay must reply `200 text/plain` with `X` within ~10 s.
- Notifications must be answered `202` quickly (~3 s) or Graph retries and
  eventually drops the subscription. Acknowledge first, forward after.
- Notification body has `value: [ { subscriptionId, clientState, changeType,
  resource, resourceData: { id } } ]`. Chat id is parsed from `resource`
  (`chats('<chatId>')/messages('<msgId>')`).

## 4. Relay API

Transport: HTTPS + WSS behind a TLS terminator (Caddy with Let's Encrypt, or
a Cloudflare Tunnel so no inbound port is opened).

| Endpoint | Purpose |
|---|---|
| `POST /register` | Client gets `{deviceId, deviceSecret}`. Store locally (0600). |
| `GET /ws?deviceId=..` | WebSocket. Auth: `Authorization: Bearer <deviceSecret>`. |
| `POST /hook/{deviceId}` | Graph notifications and the validation handshake. |
| `POST /lifecycle/{deviceId}` | Graph lifecycle notifications. |
| `GET /healthz` | Liveness for monitoring. |

### WebSocket messages (JSON, server -> client unless noted)

    {"type":"hello","heartbeatSec":20}
    {"type":"heartbeat"}
    {"type":"message","chatId":"19:...","messageId":"1700000000000"}
    {"type":"resync"}            // lifecycle "missed" or relay restart
    {"type":"reauth"}            // client must renew/recreate subscription

Client -> server: `{"type":"pong"}` optional; plain WebSocket ping is enough.

## 5. Client behaviour (contract with penguin-ms-suite)

The app already contains the seam: `src/relay.rs` and the scheduler in
`src/pages/teams.rs`.

- `relay::start()` returns a stream of `PushEvent::{Message{chat_id}, Resync}`.
- `relay::set_healthy(true)` on connect and on each heartbeat;
  `set_healthy(false)` on disconnect or after ~45 s without a heartbeat.
- While healthy: chat-list poll slows to a 60 s safety net, the 2 s open-chat
  poll stops, and each event triggers an immediate refresh.
- While unhealthy (or no relay configured): full adaptive polling resumes with
  no other change. This is the fallback.
- On every (re)connect: send nothing, treat as `Resync` and do a full refresh,
  because notifications may have been missed while disconnected.
- Reconnect with exponential backoff (1 s .. 60 s) plus jitter.
- Config: `PENGUIN_MS_RELAY_URL`, e.g. `https://relay.example.com`.

## 6. Failure modes

| Failure | Effect | Handling |
|---|---|---|
| Relay down | No pushes | `healthy=false` within ~45 s, polling fallback |
| WebSocket drops | Gap in events | Reconnect, `Resync` |
| Graph notification lost | Missed message | Safety-net poll (60 s), `missed` lifecycle event |
| Subscription expires | Pushes stop silently | Client renews at ~50% lifetime; `reauth` event |
| Duplicate notification | Double refresh | Harmless; client dedups by message id |
| Client offline | No delivery | Subscription lapses; full refresh on return |
| Relay restart | Hub map lost | Clients reconnect, `Resync` |

## 7. Security

- HTTPS/WSS only. `deviceSecret` is random 256-bit; compare in constant time.
- Verify `clientState` on every Graph notification.
- Ids only on the wire. Logs must not contain tokens or message content.
- Rate-limit `/register` and `/hook/*`. Cap connections per device (1).
- Relay holds no Graph tokens (option B). A compromised relay can leak which
  chats are active and when, and can fake events (causing refreshes), but
  cannot read messages.
- Restrict `/register` (shared invite secret or allowlist) if the relay is
  Internet-facing.

## 8. Hosting and cost

- Single small binary (Rust with axum, or Go). Idle memory in the low MBs.
- One small VPS or container is enough for a team. Cloudflare Tunnel avoids
  opening ports. Cost is the hosting only (a few EUR/month); Microsoft
  charges nothing for basic delegated notifications [verify].

### Variant C: Azure Event Hubs, no relay server

Graph can deliver to Event Hubs instead of a webhook. The desktop client could
read the hub directly over AMQP/WebSocket, so no public endpoint is needed.
Costs: Event Hubs pricing, SAS key distribution to clients, and one hub
consumer group per user. Worth comparing if a server cannot be hosted.

## 9. Build checklist for the new project

1. Relay: `/register`, `/ws`, `/hook`, `/lifecycle`, `/healthz`, in-memory hub.
2. Validation handshake and 202-fast acknowledge.
3. Heartbeat (20 s) and dead-connection reaping (45 s).
4. Integration test with a fake Graph POST and a test WebSocket client.
5. Client side in penguin-ms-suite: implement `relay::start()` (WebSocket
   with reconnect), Graph subscription create/renew (`POST/PATCH
   /subscriptions`), call `set_healthy`.
6. Deploy behind TLS; set `PENGUIN_MS_RELAY_URL`.
7. Re-verify every [verify] item against current Microsoft docs.
