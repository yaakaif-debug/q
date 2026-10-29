# Hyperliquid (core + HIP-3 dexes) — market-data reference for M0

Scope: public market data only (no signing, no `/exchange`, no user endpoints).
All findings checked on **2026-10-29** against the official docs and, where noted,
against live `https://api.hyperliquid.xyz` responses fetched the same day.
Anything not confirmed by both docs or a live response is marked **UNVERIFIED**.

## Endpoints

| Item | Value | Source |
| --- | --- | --- |
| REST info | `POST https://api.hyperliquid.xyz/info`, JSON body `{"type": ...}` | [Info endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint) |
| WebSocket | `wss://api.hyperliquid.xyz/ws` (testnet `wss://api.hyperliquid-testnet.xyz/ws`) | [Websocket](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket) |

### REST `info` request types used by M0

| `type` | Params | Use | Weight |
| --- | --- | --- | --- |
| `perpDexs` | — | Dex discovery. Element 0 is `null` (= core, dex `""`), then one object per HIP-3 dex: `name`, `fullName`, `deployer`, `oracleUpdater`, `feeRecipient`, `assetToStreamingOiCap`, `assetToFundingMultiplier`; live response also has `subDeployers`, `assetToFundingInterestRate`, `assetToFundingClamp`. | 20 |
| `meta` / `metaAndAssetCtxs` | `dex` (`""` = core) | Universe per dex. Live fields per asset: `name`, `szDecimals`, `maxLeverage`, `marginTableId`, `onlyIsolated`, `marginMode` (`strictIsolated` / `noCross`), `isDelisted`, and on HIP-3 also `growthMode`, `deployerFeeScale`, `lastFeeScaleChangeTime`, `lastGrowthModeChangeTime`. Top level: `universe`, `marginTables`, `collateralToken` (spot token index). Asset ctx: `funding`, `openInterest`, `oraclePx`, `markPx`, `midPx`, `premium`, `impactPxs`, `dayNtlVlm`, `prevDayPx`, `dayBaseVlm`. | 20 |
| `spotMeta` | — | Resolve `collateralToken` index to a token name (USDC, USDH, USDE ...). | 20 |
| `fundingHistory` | `coin`, `startTime`, `endTime?` | Settled funding: `[{coin, fundingRate, premium, time}]`. HIP-3 coins use the `dex:COIN` name. | 20 + 1 per 20 items |
| `predictedFundings` | — | Predicted funding + `nextFundingTime` per venue. **Core dex only** (docs: "only supported for the first perp dex"). Live responses (2026-10-29) also carry `fundingIntervalHours` per venue (`HlPerp: 1`), which the docs' example omits. | 20 |
| `l2Book` | `coin`, `nSigFigs?`, `mantissa?` | Book snapshot (REST fallback). | 2 |

Live check 2026-10-29: `perpDexs` returned core + 10 HIP-3 dexes
(`xyz, flx, vntl, hyna, km, abcd, cash, para, mkts, io`). Every asset of `flx`,
`vntl`, `hyna`, `km`, `cash` and `abcd` was `isDelisted` that day; active assets:
core 179, `xyz` 119, `para` 33, `io` 12, `mkts` 5. Collateral tokens: core, `xyz`,
`para`, `mkts`, `io`, `abcd` → USDC (0); `flx`, `vntl`, `km` → USDH (360);
`hyna` → USDE (235); `cash` → USDT0 (268). The adapter never uses this list: it
calls `perpDexs` at startup and every hour.

## WebSocket channels used by M0

Subscribe: `{"method":"subscribe","subscription":{...}}`; ack:
`{"channel":"subscriptionResponse","data":{...}}`; data messages:
`{"channel":"<type>","data":...}`. Source: [Subscriptions](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions).

| Subscription | Data | Notes |
| --- | --- | --- |
| `{"type":"l2Book","coin":C}` | `WsBook {coin, levels:[bids, asks], time}`; `WsLevel {px, sz, n}` | **Snapshot feed**: every message is the full top-20 book (5 with `fast: true`), pushed per block, at least 0.5 s apart. No sequence numbers. |
| `{"type":"trades","coin":C}` | `WsTrade[] {coin, side, px, sz, hash, time, tid, users}` | `side`: `"B"` (buy aggressor) / `"A"` (sell aggressor) — **UNVERIFIED** meaning of aggressor side; docs only say `side: string`. Unique id = `(time, coin, tid)`. |
| `{"type":"bbo","coin":C}` | `WsBbo {coin, time, bbo:[bid|null, ask|null]}` | Sent only when BBO changes on a block. Off by default in M0 (subscription budget, see below). |
| `{"type":"allDexsAssetCtxs"}` | `{"ctxs": [[dex, [ctx, ...]], ...]}` | One subscription covers predicted funding, mark/oracle and OI for core and every HIP-3 dex. Docs name the type only (`WsAllDexsAssetCtxs`); shape taken from live captures 2026-10-29: contexts are in the order of that dex's `meta.universe`, so the adapter maps index → coin through the latest metadata. Observed cadence ~1 message per 10–15 s (~140 KB each). |
| `{"type":"activeAssetCtx","coin":C}` | `{coin, ctx: PerpsAssetCtx}` | Not used (allDexsAssetCtxs is cheaper). |

Coin naming: core uses `BTC`; HIP-3 uses `dex:COIN`, e.g. `xyz:NVDA`
([Asset IDs](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/asset-ids)).
Asset id for HIP-3 = `100000 + perp_dex_index * 10000 + index_in_meta`.

Heartbeat: server closes a connection that has not received a message for 60 s;
client sends `{"method":"ping"}`, server answers `{"channel":"pong"}`
([Timeouts and heartbeats](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/timeouts-and-heartbeats)).
On reconnect the subscription ack carries a fresh snapshot.

## Rate limits (per IP)

Source: [Rate limits and user limits](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/rate-limits-and-user-limits).

- REST: aggregated weight 1200 / minute. `l2Book`, `allMids`, `exchangeStatus` … weight 2; most other `info` 20; `fundingHistory` etc. +1 per 20 items returned.
- WebSocket: max 10 connections, 30 new connections / minute, **1000 subscriptions**, 2000 client messages / minute, 100 in-flight posts.
- All HL venue instances (core and every HIP-3 dex) share these limits because they apply per IP.

Consequence for M0: with ~180 active core coins and ~280 HIP-3 coins (live,
2026-10-29), `l2Book` + `trades` for every coin is ~920 subscriptions, close to
the 1000 cap. The adapter assigns subscriptions by priority
(`allDexsAssetCtxs` → `l2Book` → `trades` → `bbo`) up to
`max_subscriptions` (default 950) and reports uncovered coins in logs and the
`md_uncovered_streams` gauge.

## Tick size, lot size, fees, funding interval — where they come from

- **Lot size**: `szDecimals` per asset → step `10^-szDecimals` ([Tick and lot size](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/tick-and-lot-size)).
- **Price rule**: not a fixed tick. Up to 5 significant figures and at most `MAX_DECIMALS - szDecimals` decimals, `MAX_DECIMALS = 6` for perps; integer prices always valid. Stored as `PriceRule::SigFigs`.
- **Fees**: per-user tier (14-day volume) from `userFees`, which needs an account address → not available from public metadata; M0 records base-tier inputs only. Public per-asset fee inputs that M0 does record: `deployerFeeScale` and `growthMode` (HIP-3; present in live `meta`, not in the docs' example). Formula in [Fees](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees): HIP-3 scale = `feeScale + 1` if `feeScale < 1` else `2 * feeScale`; growth mode ×0.1.
- **Funding interval**: not in `meta`. Core: `predictedFundings[*].HlPerp.fundingIntervalHours` (live field, see above). HIP-3: **inferred** per dex from consecutive `fundingHistory` timestamps (most common spacing across the dex's coins, first coin of each dex polled first). Docs ([Funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding)) say funding is paid every hour and "the funding cap and funding interval do not depend on the asset". Nothing hardcodes 1 h. Live 2026-10-29: inference gives 1 h for every dex with active assets.
- **Predicted funding**: `funding` in the asset ctx (`metaAndAssetCtxs` / `allDexsAssetCtxs`) is the current-hour rate (per interval, not annualised). `predictedFundings` gives `nextFundingTime` for core only. For HIP-3 the next settlement time is derived as last settlement + inferred interval (**UNVERIFIED** that HIP-3 settles on the same hourly grid as core).
- **Settled funding**: `fundingHistory` (REST). M0 polls every coin hourly through a weight-budgeted limiter.
- **Collateral**: `collateralToken` (spot token index) → `spotMeta.tokens[i].name`. Live 2026-10-29: core and `xyz` report `0` (USDC).
- **Margin mode**: `marginMode` / `onlyIsolated` per asset.
- **Delisting / settlement**: `isDelisted` per asset; deployer `haltTrading` settles at mark ([HIP-3](https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals)). How a halted-but-not-delisted asset appears in `meta` is **UNVERIFIED**.

## Open points (UNVERIFIED)

- `allDexsAssetCtxs` has no formal schema in the docs (parsed from live captures).
- Whether `bbo` and `l2Book` subscriptions for HIP-3 coins count against the same 1000-subscription limit as core (assumed yes, per-IP wording).
- Trade `side` letter semantics.
- Disk usage of full-universe `l2Book` recording (estimate in PR, to be measured during the 72 h run).
