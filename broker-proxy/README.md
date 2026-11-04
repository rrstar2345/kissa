# Broker Order Proxy

A small Go server that sits on a VM with a **static public IP** and places orders on behalf of many clients, so brokers that require IP whitelisting (Dhan) see one fixed source address. Clients talk to the proxy over REST and a WebSocket; the proxy talks to the broker.

Brokers today: **Dhan** (v2 REST + live order WebSocket) and **Paper** (simulated). Adding another broker is one package.

## Architecture

```
 clients ──REST (orders)──►┐                         ┌─► Dhan REST  (static IP)
 clients ◄─WSS (updates)───┤   proxy (this server)   │◄─ Dhan live order WSS
                           │  auth → session → broker┤
                           └─────────────────────────┴─► Paper (in-memory)
```

**Transport: REST for commands + one WSS per client for updates.** Each order is a self-contained request/response, so it is easy to retry, debug and make idempotent, and a dropped socket can never lose or duplicate a command. Live order updates are pushed over the WSS. This also fits the free-form concurrency you expect: requests from many clients are served in parallel with no shared locks beyond one session.

**Sessions (credentials in memory only).**
1. Client calls `POST /v1/sessions` with its API key plus the broker credentials. The proxy opens the broker connection (for Dhan: verifies the token, starts the live-order feed) and returns a `sessionId`.
2. Every later call carries `X-API-Key` and `X-Session-Id`. A session belongs to the API key that created it; other clients get 404.
3. A session ends on `DELETE /v1/sessions/{id}`, on idle timeout (no REST use and no WSS attached), or at max lifetime. Ending it stops the feed, drops the connection object (and credentials) and closes attached sockets.

**Nothing is persisted.** No database, no audit file, no credentials on disk, and the access log records only method, path, status and client id (never headers, query strings or bodies). Because the server forgets everything, **the client is the system of record**: place/modify/cancel responses return the full order snapshot, `GET /v1/orders` and `/v1/trades` return the broker's books, and every WSS event carries the full normalized order plus the broker's raw payload.
Go strings are immutable, so credentials cannot be zeroed in RAM; they are released for garbage collection when the session ends.

**One upstream feed per session, fan-out to its sockets.** Each session runs one Dhan order feed (auto-reconnect with backoff, ping/keepalive) and a hub that stamps events with a per-session `seq`, keeps the last N (`replayBuffer`) and delivers to every WSS attached to that session. Reconnect with `?since=<lastSeq>` to replay what you missed; if it is older than the buffer you get a `gap` notice and should resync from `GET /v1/orders`. A consumer that falls behind is dropped (never blocks the broker feed) and reconnects the same way.
Note: two sessions for the *same* Dhan account open two upstream feeds; reuse one session per account.

**Safety rules for orders**
- **No market orders.** `MARKET` and `STOP_LOSS_MARKET` are rejected (`market_order_not_allowed`). Only `LIMIT` and `STOP_LOSS` with a positive `price` are accepted.
- Strict JSON: unknown fields are rejected, so a typo cannot silently become a zero.
- **Idempotency via `correlationId`** (within a session): a retry of an accepted order returns the original ack with `Idempotent-Replay: true` and never reaches the broker twice, even under concurrent retries. If an earlier attempt timed out or the broker was unreachable, retries get `409 outcome_unknown`; check `GET /v1/orders/external/{correlationId}` first.
- Order-changing calls are not aborted when the client disconnects mid-flight.
- Dhan order calls are rate-limited per session (`dhan.ordersPerSecond`, default 10).

## Code layout

```
cmd/proxy/main.go             wiring, graceful shutdown, `keygen`
internal/config               server config (clients' key hashes, timeouts, Dhan URLs)
internal/auth                 API-key authentication (SHA-256 hashes only)
internal/model                broker-neutral order/trade/event types, validation, errors
internal/broker               Conn / Opener interfaces + registry   ← the extension point
internal/broker/dhan          REST client, rate limiter, live-feed WSS, payload mapper
internal/broker/paper         in-memory simulated broker (pluggable Executor)
internal/session              session manager, event hub (seq + replay), idempotency
internal/server               routes, handlers, WSS endpoint, middleware
```

**Adding a broker:** create `internal/broker/<name>` implementing `broker.Conn`, expose `Open(ctx, creds, sink) (broker.Conn, error)`, push live events into `sink`, and add one `reg.Register(...)` line in `main.go`. Nothing else changes.

**Wiring real paper trading later:** `paper.New(sink, executor)` takes an `Executor` that decides if/when/at what price an order fills. Today `LimitFill` fills `LIMIT` orders instantly at the limit price and leaves `STOP_LOSS` pending (no market data yet to trigger them).

## API

All endpoints except `/healthz` need `X-API-Key`. Order/trade/WSS endpoints also need `X-Session-Id`. Bodies and fields mirror [Dhan v2 Orders](https://dhanhq.co/docs/v2/orders/); `dhanClientId` is filled from the session (anything you send is overwritten).

| Method & path | Purpose |
|---|---|
| `POST /v1/sessions` | `{"broker":"dhan","credentials":{"clientId":"…","accessToken":"…"}}` or `{"broker":"paper"}` → `201 {sessionId, …}` |
| `DELETE /v1/sessions/{id}` | End session, drop credentials |
| `POST /v1/orders` | Place order (`orderId`, `orderStatus`, `broker`, `order` snapshot) |
| `PUT /v1/orders/{orderId}` | Modify |
| `DELETE /v1/orders/{orderId}` | Cancel (`202`) |
| `POST /v1/orders/slicing` | Slice (F&O freeze limits) |
| `GET /v1/orders`, `/v1/orders/{orderId}`, `/v1/orders/external/{correlationId}` | Order book / by id / by correlation id |
| `GET /v1/trades`, `/v1/trades/{orderId}` | Trade book / trades of an order |
| `GET /v1/ws[?since=N]` | WebSocket: live events |
| `GET /healthz` | Liveness, enabled brokers |

Place example:
```bash
curl -X POST https://HOST:8443/v1/orders -H "X-API-Key: $KEY" -H "X-Session-Id: $SID" -d '{
  "transactionType":"BUY","exchangeSegment":"NSE_EQ","productType":"INTRADAY",
  "orderType":"LIMIT","validity":"DAY","securityId":"11536","quantity":5,
  "price":3500.5,"correlationId":"strat1-0001"}'
```

Errors: `{"error":{"code","message","detail"}}`; `detail` carries Dhan's raw error body. Broker auth failures are `424 broker_unauthorized`.

WSS events (one JSON object per frame):
```json
{"type":"order_update","seq":7,"time":"…","broker":"dhan",
 "order":{ "orderId":"…","orderStatus":"TRADED","transactionType":"BUY","productType":"INTRADAY", … },
 "raw":{ …Dhan's original payload… }}
{"type":"feed_status","seq":8,"broker":"dhan","state":"disconnected","detail":"…"}
{"type":"session","state":"subscribed" | "gap"}
```
Dhan's coded fields (`B`, `I`, `LMT`, …) are translated to the REST vocabulary (`BUY`, `INTRADAY`, `LIMIT`, …). `PENDING` with a partial fill is reported as `PART_TRADED`.

## Run on the Oracle Cloud Ubuntu VM

1. **Static IP:** attach a *Reserved Public IP* to the VM's VNIC and whitelist it in Dhan. Use the reserved IP for outbound too (it is the instance's only public IP).
2. **Build:** `go build -o proxy ./cmd/proxy` (or cross-compile: `GOOS=linux GOARCH=arm64` for Ampere shapes).
3. **Keys:** `./proxy keygen` → give the key to the client, put the hash in `config.json` (copy `config.example.json`).
4. **TLS is required in production** (credentials and keys are in request bodies/headers). Either set `tls.certFile`/`tls.keyFile`, or put Caddy/nginx in front and bind the proxy to `127.0.0.1`.
5. **Open the port twice:** VCN security list / NSG ingress, *and* the VM's iptables (Oracle's Ubuntu images block everything but SSH by default): `sudo iptables -I INPUT -p tcp --dport 8443 -j ACCEPT` then persist with `netfilter-persistent save`.
6. **Run as a service** (`/etc/systemd/system/broker-proxy.service`):
   ```ini
   [Unit]
   Description=Broker order proxy
   After=network-online.target
   [Service]
   ExecStart=/opt/broker-proxy/proxy -config /opt/broker-proxy/config.json
   Restart=always
   User=proxy
   NoNewPrivileges=true
   [Install]
   WantedBy=multi-user.target
   ```
   A restart drops all sessions by design; clients must open a new one and resync from the order book.

Tests: `go test -race ./...` (paper flow, idempotency under concurrency, WSS replay, and Dhan against a mock REST + feed server).

## Known limits / next steps
- Dhan's docs do not define a login acknowledgement for the feed or the slicing response shape; both are handled leniently and should be confirmed against the live API.
- The feed's `connected` status means "socket open, login sent".
- Single node, in-memory sessions: horizontal scaling would need sticky routing or a shared store (which conflicts with the no-persistence rule).
- No per-client request rate limiting yet (only the Dhan order limiter).
