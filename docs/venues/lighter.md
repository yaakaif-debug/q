# Lighter — market-data reference for M0

Scope: public market data only (no signer, no `sendTx`, no auth tokens).
Checked on **2026-10-29** against the official docs at `docs.lighter.xyz`
(the spec's `apidocs.lighter.xyz/docs/environments` now redirects there) and
against live mainnet responses fetched the same day. Unconfirmed items are
marked **UNVERIFIED**.

## Endpoints

| Item | Value | Source |
| --- | --- | --- |
| REST | `https://mainnet.zklighter.elliot.ai/api/v1/` | [Environments](https://docs.lighter.xyz/developers/getting-started/environments) |
| WebSocket | `wss://mainnet.zklighter.elliot.ai/stream` (read-only variant for restricted regions: `?readonly=true`) | same |
| Testnet | REST `https://testnet.zklighter.elliot.ai`, WS `.../stream`, chain id 300 | same |
| Mainnet chain id | 304 (signing only; unused in M0) | same |

Colocation hint from the docs: AWS Tokyo `ap-northeast-1`, AZ `apne1-az4`.
Traffic goes through AWS CloudFront.

Verified 2026-10-29 from a US egress: `wss://.../stream` (without `readonly`)
fails the handshake with HTTP 400 and `{"code":20558,"message":"You are accessing
Lighter from a restricted jurisdiction..."}`; `?readonly=true` works. REST market
data endpoints answer normally. The adapter uses `?readonly=true` by default.

### REST used by M0

| Path | Use | Notes |
| --- | --- | --- |
| `GET /api/v1/orderBookDetails?filter=perp` | Market metadata | `order_book_details[]`: `symbol`, `market_id`, `status` (`active`/`inactive`), `taker_fee`, `maker_fee` (percent strings, market base rate), `min_base_amount`, `min_quote_amount`, `size_decimals`, `price_decimals`, `supported_*_decimals`, `default/min_initial_margin_fraction`, `maintenance_margin_fraction`, `funding_clamp_small`, `funding_clamp_big`, `base_interest_rate`, `funding_premium_multiplier`, `mark_price`, `index_price`, `open_interest` (single-sided, base), `open_interest_usd`, `market_config.{market_margin_mode, force_reduce_only, trading_hours, hidden}`, `created_at` (ms string). [orderBookDetails](https://docs.lighter.xyz/api/orders/orderbookdetails) |
| `GET /api/v1/fundings?market_id&resolution=1h&start_timestamp&end_timestamp&count_back` | Settled funding history | `fundings[] {timestamp (s), value, rate, direction}`. [fundings](https://docs.lighter.xyz/api/funding/fundings) |
| `GET /` | Status `{status, network_id, timestamp}` | [status](https://docs.lighter.xyz/api/system/status). Note: `/api/v1/status` returns 403 from CloudFront. |

Live check 2026-10-29: 241 perp markets, 213 `active`.

## WebSocket channels used by M0

Envelope: `type: "connected"`, `type: "subscribed/<channel>"` (snapshot),
`type: "update/<channel>"`, errors as `{error:{code,message}}`. Subscribe with
`{"type":"subscribe","channel":"order_book/0"}`; response `channel` uses a colon:
`order_book:0`. Source: [WebSocket](https://docs.lighter.xyz/api/websocket).

| Channel | Payload | Sequencing |
| --- | --- | --- |
| `order_book/{market_id}` | `order_book {asks[], bids[] of {price,size}, offset, nonce, begin_nonce, last_updated_at}`, envelope `timestamp` (ms) | Snapshot on subscribe, then deltas every 50 ms; `size` is the new total, `"0"` removes. **Continuity: `begin_nonce` of an update must equal `nonce` of the previous message.** `offset` is per API server and may jump on reconnect — do not use it. On a gap: unsubscribe + subscribe for a fresh snapshot ([Market data guide](https://docs.lighter.xyz/developers/integration-guides/market-data)). |
| `trade/{market_id}` | `{trades[], liquidation_trades[], nonce}`; `Trade {trade_id, type, market_id, size, price, usd_amount, is_maker_ask, timestamp (ms), transaction_time (µs), ...}` | No sequence check needed (trades are not state). Aggressor side: `is_maker_ask == true` → maker was the ask → buy aggressor. **UNVERIFIED** (inferred from the field name). |
| `market_stats/all` | `market_stats {symbol, market_id, index_price, mark_price, mid_price, open_interest, current_funding_rate, funding_rate, funding_timestamp, ...}` | `current_funding_rate` = estimate of the upcoming payment; `funding_rate` = last settled at `funding_timestamp`. |
| `ticker/{market_id}` | BBO `{s, a{price,size}, b{price,size}}` + `nonce` | Per nonce. Off by default in M0 (BBO derives from `order_book`; saves client messages). |

Payload of `market_stats/all` is a map keyed by market id (verified live
2026-10-29; the docs show only the single-market form; the adapter accepts both).
Live quirks handled by the parser: `mid_price` is `""` when a book is one-sided
(each map entry is parsed separately, so one bad market never drops the rest);
`open_interest` here is single-sided **USD** (`open_interest_usd = 2 *
open_interest`), unlike `orderBookDetails.open_interest`, which is in base tokens.

Timestamps ([Timestamps](https://docs.lighter.xyz/developers/reference/timestamps)):
envelope `timestamp` ms (API server), `Trade.timestamp` ms (block), `transaction_time`
and book `last_updated_at` µs. `fundings[].timestamp` is in **seconds** (live).

Keepalive: client must send a frame at least every 2 minutes; `{"type":"ping"}` →
`{"type":"pong"}`. `permessage-deflate` supported. Slow clients are dropped after
250 ms write timeouts.

## Rate limits

Source: [Rate limits](https://docs.lighter.xyz/developers/reference/rate-limits).

- REST, Standard / unauthenticated: **60 requests per rolling minute** per IP. Opening `/stream` counts against it (weight 90 for weighted tiers).
- WebSocket per IP: 255 connections, 500 subscriptions per connection, 5000 per IP, **200 client messages per minute**, 50 in-flight.
- Exceeding: HTTP 429 (or 405); WS disconnection. Firewall cooldown 60 s.

Consequence for M0: subscribing `order_book` + `trade` for ~213 markets is ~430
client messages, i.e. at least ~2.2 minutes of subscribing under the 200/min cap.
The adapter sends all Lighter client messages (subscribe, resync, ping) through one
shared limiter (default 150 / minute, burst 10). A burst of ~150 subscribe
messages produced `{"error":{"code":30010,"message":"Too Many Inflight Messages!"}}`
and a connection reset on 2026-10-29 — hence the small burst.

## Tick size, lot size, fees, funding interval — where they come from

- **Tick / lot**: integer encoding with `price_decimals` / `size_decimals` → tick `10^-price_decimals`, lot `10^-size_decimals`; minimums `min_base_amount`, `min_quote_amount` ([Market data](https://docs.lighter.xyz/developers/integration-guides/market-data)).
- **Fees**: `maker_fee` / `taker_fee` per market in `orderBookDetails` (percent). Account tier then applies: docs (2026-10-29) say Standard pays 0 / 0 on regular orders with **taker latency 300 ms, maker latency 0 ms, cancel/modify 300 ms**; Premium 0.0040% / 0.0280% at 0 LIT with 140 ms taker latency ([Trading fees](https://docs.lighter.xyz/trading/trading-fees)). This **differs from SPEC.md**, which says Standard has a 200 ms maker delay and Premium has none — flagged as an open question.
- **Funding interval**: not a metadata field. Docs: "Funding payments occur at each hour mark" ([Funding](https://docs.lighter.xyz/trading/funding)). The adapter infers the interval per market from consecutive settlement timestamps — at startup via `fundings` (rate-limited) and continuously from `market_stats.funding_timestamp` changes. Never hardcoded.
- **Funding rate units**: `fundings[].rate` and `market_stats.*funding_rate` are in **percent per interval**. Verified live 2026-10-29: market 0 `rate "0.0012"`, `value "0.028254"` = 0.0012% × ETH index ≈ 2354. Normalised events carry the fraction (`rate / 100`).
- **Funding direction**: `fundings[].direction` is `long`/`short` — **UNVERIFIED** whether it names the paying side.
- **Clamp / interest**: `funding_clamp_small`, `funding_clamp_big`, `base_interest_rate` (percent) per market.
- **Collateral**: USDC (perps). Multi-asset margin exists ([Multi-asset margin](https://docs.lighter.xyz/trading/multi-asset-margin)) — not used in M0.

## Open points (UNVERIFIED)

- Trade aggressor side from `is_maker_ask`.
- Whether the 200 messages/min client cap counts WS protocol ping frames.
- Maker-delay discrepancy between SPEC.md and current docs (see above).
