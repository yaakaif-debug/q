# Aster (futures v3) — market-data reference for M0

Scope: public market data only (no API wallet, no EIP-712, no signed endpoints).
Checked on **2026-10-29** against the official docs and live
`https://fapi.asterdex.com` responses fetched the same day. Unconfirmed items
are marked **UNVERIFIED**.

Primary source: [Futures API v3 (EN), GitHub `asterdex/api-docs`](https://github.com/asterdex/api-docs/blob/master/V3(Recommended)/EN/aster-finance-futures-api-v3.md)
(rendered at [asterdex.github.io/aster-api-website/futures-v3/general-info](https://asterdex.github.io/aster-api-website/futures-v3/general-info/)).
The protocol is Binance-USDⓈ-M-compatible in shape (`X-MBX-*` headers, `U/u/pu` depth ids).

## Endpoints

| Item | Value |
| --- | --- |
| REST | `https://fapi.asterdex.com` (v3 paths `/fapi/v3/...`) |
| WebSocket | `wss://fstream.asterdex.com` — raw `/ws/<stream>` or combined `/stream?streams=a/b/c` (combined wraps as `{"stream":..., "data":...}`) |
| Testnet | `https://fapi.asterdex-testnet.com` (SPEC.md; not used in M0) |

### REST used by M0

| Path | Weight | Use |
| --- | --- | --- |
| `GET /fapi/v3/exchangeInfo` | 1 | Symbols, filters, `rateLimits`. Per symbol: `symbol`, `status` (`TRADING` ...), `contractType` (`PERPETUAL`), `baseAsset`, `quoteAsset`, `marginAsset`, `onboardDate`, `filters` (`PRICE_FILTER.tickSize`, `LOT_SIZE.stepSize/minQty`, `MIN_NOTIONAL.notional` ...). `pricePrecision`/`quantityPrecision` must **not** be used as tick/step (docs). |
| `GET /fapi/v3/fundingInfo` | 1 | Per symbol `fundingIntervalHours`, `interestRate`, `fundingFeeCap`, `fundingFeeFloor`. **The funding interval source.** |
| `GET /fapi/v3/premiumIndex` | 1 | `markPrice`, `indexPrice`, `lastFundingRate`, `nextFundingTime`, `interestRate`, `time`. |
| `GET /fapi/v3/fundingRate?symbol&startTime&endTime&limit` | 1 | Settled funding history `{symbol, fundingRate, fundingTime}`. |
| `GET /fapi/v3/depth?symbol&limit` | 2 (≤50), 5 (100), 10 (500), 20 (1000) | Snapshot for local book resync (`lastUpdateId`, `E`, `T`, `bids`, `asks`). |
| `GET /fapi/v1/openInterest?symbol` | **UNVERIFIED** | `{symbol, openInterest, time}`. Not in the v3 doc; works live on both `/fapi/v1` and `/fapi/v3` (2026-10-29). |
| `GET /fapi/v3/time`, `/fapi/v3/ping` | 1 | Latency probe. |

Live check 2026-10-29: `exchangeInfo` lists 644 symbols, 602 with `status=TRADING`.
`marginAsset` values seen: `USDT`, `USDC`, `USD1`, `U` — **not every contract is
USDT-margined**, contrary to the SPEC.md venue table. Collateral is taken from
`marginAsset` per symbol.

## WebSocket streams used by M0

Source: "Websocket Market Streams" section of the v3 doc.

| Stream | Payload | Notes |
| --- | --- | --- |
| `<symbol>@depth@100ms` | `depthUpdate {E, T, s, U, u, pu, b[[px,qty]], a[[px,qty]]}` | Diff depth. Quantities are absolute; 0 removes. |
| `<symbol>@aggTrade` | `aggTrade {E, s, a, p, q, f, l, T, m}` | `m = true` → buyer is maker → sell aggressor. |
| `!markPrice@arr@1s` | `[markPriceUpdate {E, s, p (mark), i (index), P, r (funding rate), T (next funding time)}]` | All symbols, 1 s. Predicted funding + mark + index. |
| `!bookTicker` | `bookTicker {u, E, T, s, b, B, a, A}` | All-symbol BBO, real time. |

Local book procedure (docs "How to manage a local order book correctly"):
buffer stream events → GET `/fapi/v3/depth` → drop events with `u < lastUpdateId`
→ first applied event must satisfy `U <= lastUpdateId <= u` → afterwards each
event's `pu` must equal the previous event's `u`, otherwise resync from a new
snapshot.

Connection rules: one connection lives at most **24 h**; server sends a ping frame
every 5 minutes and drops the connection if no pong within 15 minutes; at most
**10 incoming client messages per second** per connection; at most **200 streams
per connection**. Symbols in stream names are lowercase.

M0 subscribes through the combined-stream URL (no SUBSCRIBE messages), at most
`streams_per_connection` (default 180) per connection, and reconnects every
connection before the 24 h mark (`max_connection_age`, default 23 h 30 m).
The maximum number of connections per IP is **UNVERIFIED** (not documented).

## Rate limits

- `exchangeInfo.rateLimits` (live 2026-10-29): `REQUEST_WEIGHT` 2400 / minute, `ORDERS` 1200 / minute and 300 / 10 s. Limits are per IP. 429 → back off; repeated → 418 IP ban (2 minutes to 3 days). `X-MBX-USED-WEIGHT-1M` header reports usage.
- M0 runs all Aster REST calls through one weight limiter (default 1200 / minute, half of the published limit).

## Tick size, lot size, fees, funding interval — where they come from

- **Tick**: `PRICE_FILTER.tickSize`; **lot**: `LOT_SIZE.stepSize`, `minQty`; **min notional**: `MIN_NOTIONAL.notional`.
- **Funding interval**: `fundingInfo.fundingIntervalHours` per symbol (live values include 4 and 8).
- **Predicted funding**: `markPriceUpdate.r` / `premiumIndex.lastFundingRate` (fraction per interval). Despite the name it is the in-progress (predicted) rate: live 2026-10-29 BTCUSDT showed `lastFundingRate = 0.00005371` (same as `r`) while the last three settled `fundingRate` entries were `0.0001`. Settled values come from `fundingRate` history.
- **Fees**: only via `GET /fapi/v3/commissionRate` (USER_DATA, signed) → not available in M0. SPEC.md's 0% / 0.04% is not taken from metadata and is not used. Flagged for M1.
- **Collateral**: `marginAsset` per symbol.

## Keys and permissions

Not touched in M0. Per the docs' README (2026-10-29): since 2026-09-01 all
authenticated v3 endpoints require the main wallet to have made a deposit;
public market data is unaffected.

## Open points (UNVERIFIED)

- Weight of `openInterest`.
- Max WebSocket connections per IP.
- Whether `@depth@100ms` is available for every symbol (docs: "100 ms (if existing)").
