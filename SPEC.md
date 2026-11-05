# Spread Hunter — Rust Delta-Neutral Spread Engine: Spec for Claude Code

Oct 29, 2026 · @kefir

## Overview

Build `spread-hunter`, a Rust daemon that earns a delta-neutral yield on about $100k of stablecoins from funding-rate and price spreads between perpetual DEX venues. The target is 10–20% APR on total capital, net of fees, slippage and rebalancing costs. Milestone M1 must show from recorded data whether this size can reach that target before any capital is committed.

**Scope (this version)**

- Venues: Hyperliquid core perps, Hyperliquid HIP-3 dexes (each dex is its own venue in the code), Lighter, Aster. DEX only: no centralized-exchange adapters anywhere.
- Confirmed pairs: Hyperliquid core ↔ HIP-3 and HIP-3 ↔ HIP-3; Hyperliquid ↔ Lighter; Aster as a third leg against either.
- Strategies first: cross-venue funding carry (S1) and intra-Hyperliquid cross-dex carry (S2), both executed maker-first (S5 tactic). Spot–perp carry (S3) and price-spread convergence (S4) come later.
- Run modes: `record`, `scan` (read-only opportunity report), `paper`, `live`.

**Non-goals**

- No CEX, no prediction markets, no directional trading, no leverage above configured caps, no microsecond HFT.
- No automated withdrawals. The bot plans cross-venue rebalancing; the operator signs and sends the transfers.

**Return math the engine must compute and report per opportunity**

```latex
\mathrm{ROC} = \frac{s_{net} \cdot N}{C}, \qquad C = \frac{N}{L_a} + \frac{N}{L_b} + R
```

Here s\_net is the annualised funding spread after fees and slippage, N the notional per leg, L the leverage per leg and R the idle reserve. At L = 3 on both legs and a 15% reserve, N / C ≈ 1.27, so 15% ROC needs a sustained net spread of about 12% APR. The scanner prints this break-even spread next to every pair.

**How to work on this repo (instructions for Claude Code)**

1. Work milestone by milestone (see Milestones). Do not write live order-placement paths before M2 acceptance passes.
2. All code comments and doc comments are written in Russian. Identifiers, log messages and commit messages stay in English.
3. Ask before adding any dependency outside the approved list in Architecture.
4. Never log, print, serialise or persist secrets, including in error messages and panics.
5. Every function that touches money (sizing, fees, PnL, order quantities) has unit tests. Every venue adapter has tests against recorded JSON fixtures.
6. When a venue API detail is uncertain, read the official docs and pin the finding in `docs/venues/<venue>.md` with the URL and date, instead of guessing.

## Strategy modules

S1 and S2 are the core earners and S5 is the execution tactic both use. Every strategy implements one `Strategy` trait and only emits intents; it never talks to venues directly.

| ID | Strategy | Legs | Latency sensitivity | Milestone |
| --- | --- | --- | --- | --- |
| S1 | Cross-venue funding carry | Short where shorts are paid more, long the same underlying elsewhere; venues HL core / HIP-3, Lighter, Aster | Low (hold days) | M2–M3 |
| S2 | Intra-Hyperliquid cross-dex carry | Same logic between HL core and a HIP-3 dex, or between two HIP-3 dexes | Low | M2–M3 |
| S3 | Spot–perp carry | Long HL spot + short HL perp | Low | M5 |
| S4 | Price-spread convergence | Long the cheap venue, short the rich one, exit at the mean | High | M5 |
| S5 | Maker-first execution | Post-only on the lower-maker-fee leg, IOC hedge on the other | Medium | Inside S1 and S2 from M2 |
| S6 | Funding rate lock | Pendle Boros Yield Units against the Hyperliquid leg | Low | M6, optional, manual approval |

### S1 — cross-venue funding carry

Hold equal base quantity short on the venue that pays shorts more and long on the venue with lower (or negative) funding. Normalise every venue's rate to APR from the interval the venue reports; never hardcode intervals.

```latex
APR_v = r_v \cdot \frac{8760}{h_v}, \qquad s = APR_{short} - APR_{long}
```

- **Signal.** Compute s three ways: current predicted rate, EMA of settled rates over 24 h, 7-day realised mean. Enter only when all three exceed `min_gross_apr` and s kept its sign for the last `k_settlements` (default 6).
- **Cost.** Entry plus exit = fees on both legs at the account's tier + order-book slippage walked at target size + expected spread paid on unwind + the planned share of rebalancing transfer costs.
- **Break-even.** H\_be = cost / (s / 8760) hours. Enter only if H\_be ≤ `max_breakeven_hours` (default 72) and expected s\_net ≥ `min_net_apr` (default 12%).
- **Exit.** s\_ema below `exit_apr` (default 3%) for `m_intervals` in a row; or another pair beats the current one by `switch_margin` after both switching costs; or any risk trigger.
- **Pair eligibility.** Map instruments to a canonical underlying ID through `config/symbols.toml` plus auto-matching that needs operator approval. Reject pairs whose marks come from different underlyings (pre-IPO perps, differently constructed indices) unless allowlisted.
- **Collateral basis.** Hyperliquid core and Lighter margin in USDC; Aster collateral is set per contract by `marginAsset` (USDT, USDC, USD1 or U, from metadata). Each non-USDC collateral against USDC is tracked as a PnL component and in risk.

### S2 — intra-Hyperliquid cross-dex carry

HIP-3 lets several deployers list the same underlying as separate dexes, each with its own book, oracle, fees, collateral and leverage limits, so funding on one underlying diverges between dexes. S2 runs the S1 logic between HL core and a HIP-3 dex, or between two HIP-3 dexes.

- **Venue key** is `(hyperliquid, dex)`: empty string for core, dex names such as `xyz`, `flx`, `km`, `hyna` for HIP-3. The adapter discovers dexes and their metadata at startup and every hour; the list is never hardcoded.
- **Collateral per dex.** Dexes can margin in different stablecoins (USDC, USDH, USDE). If the legs use different collateral, the spot conversion on Hyperliquid is part of entry and exit cost, and the collateral pair rate is a PnL and risk component.
- **Margin mode and fees per dex** come from metadata. Some HIP-3 dexes have been isolated-only, and HIP-3 fees default to twice the core rate unless the deployer discounts them.
- **Oracle sessions.** Equity and commodity perps reprice differently on weekends and around the underlying market's open. Sizing and leverage follow a session calendar per underlying; no new S2 entry within `session_guard_min` (default 60) of a known session change.
- **Deployer actions.** Delisting or settlement at mark is detected from metadata; a pair with a settling leg moves to Exiting at once.
- **Rebalancing advantage.** Margin moves between dexes inside HyperCore without a bridge, so S2 uses a tighter rebalance threshold than S1.
- **Concentration cap.** Both legs sit on one platform, so S2 exposure is capped by `max_s2_share` (default 50% of equity).

### S3 — spot–perp carry (later)

Long spot and short perp on Hyperliquid. No cross-venue risk, but capital is C = N + N / L, so ROC is lower than S1 for the same spread. Same signal and cost model, with s = the perp leg's APR.

### S4 — price-spread convergence (later)

Trade the deviation of the cross-venue mid spread d = (mid\_a − mid\_b) / mid from its rolling mean. Enter when |d − mean| exceeds round-trip cost + `k_sigma` · σ; exit at the mean or after `max_hold`. Protocol latency bumps (Lighter Standard adds 300 ms to takers) dominate here, so S4 starts only after `latency-probe` shows a usable edge.

### S5 — maker-first execution

Post-only on the leg with the cheaper maker side (Aster 0% maker; Lighter Standard 0% with no maker delay but 300 ms on takers and on cancels/modifies, which slows repricing, or Premium for a fee with 140 ms on takers and no delay on cancels or post-only orders; HIP-3 per dex). Price it off the hedge venue's BBO minus hedge taker fee, expected slippage and `edge_bps`. On every fill, send an IOC hedge on the other venue within `hedge_timeout_ms`.

### S6 — funding rate lock (optional)

When an S1 or S2 spread is attractive but volatile, lock the Hyperliquid leg's floating funding into a fixed rate with Pendle Boros Yield Units (settled on Arbitrum). Every lock needs operator approval in the CLI.

### Backlog (separate spec addenda)

Polymarket Perps hedged liquidity rewards, pending confirmation that payouts are live; tokenized-stock spot vs HIP-3 perp carry on HyperEVM; running S2 inside a Hyperliquid user vault (the leader earns a 10% profit share and must keep at least 5% of the vault); a small prediction-market module.

## Venues and connectivity

MVP connects to four venue families from one Linux host in Tokyo. Every adapter implements the same trait, and fees, tick sizes, funding intervals, leverage caps and margin modes are read from venue metadata at startup and refreshed hourly, never taken from this table.

| Venue | Role | Base fees maker / taker | Funding interval | Collateral | Matching region | API and signing |
| --- | --- | --- | --- | --- | --- | --- |
| Hyperliquid core | S1 and S2 leg, primary | 0.015% / 0.045% | 1 h | USDC | AWS ap-northeast-1 (Tokyo) | `api.hyperliquid.xyz` REST + WS; an agent (API) wallet signs orders and cannot withdraw |
| Hyperliquid HIP-3 dexes | S2 legs, S1 leg | Default 2× core, deployers may discount (tradeXYZ shows 0.003% / 0.009%) | From metadata | Per dex: USDC, USDH, USDE | Same as core | Same API with a `dex` parameter; tickers such as `xyz:NVDA` |
| Lighter | S1 leg, S5 maker venue | Standard 0% / 0% with 0 ms maker, 300 ms taker and 300 ms cancel/modify delay; Premium 0.004% / 0.028% at 0 LIT with 140 ms taker delay and no delay on cancels or post-only orders | From API | USDC | Unknown, measure | REST `mainnet.zklighter.elliot.ai/api/v1/`, WS `/stream`, chain ID 304; signing through the official `lighter-go` shared library over FFI |
| Aster | S1 third leg, S5 maker venue | 0% / 0.04% | Per symbol, from metadata | Per contract from `marginAsset`: USDT, USDC, USD1, U | Unknown, measure | v3 REST `fapi.asterdex.com/fapi/v3`, WS `fstream.asterdex.com`; EIP-712 signature by an API wallet on behalf of the main wallet, nonce in microseconds; testnet `fapi.asterdex-testnet.com` |

As of 2026-10-29 from [arb.sh](https://arb.sh/), [HIP-3 docs](https://hyperliquid.gitbook.io/Hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals), [Lighter fees](https://docs.lighter.xyz/trading), [Lighter endpoints](https://apidocs.lighter.xyz/docs/environments), [Aster API v3](https://asterdex.github.io/aster-api-website/futures-v3/general-info/).

### Moving capital between venues

- **Hub:** an operator-controlled wallet on Arbitrum.
- **Hyperliquid ↔ Arbitrum:** a USDC withdrawal costs $1 and takes about 5 minutes ([withdraw reference](https://docs.chainstack.com/reference/hyperliquid-exchange-withdraw)).
- **Between HL core and HIP-3 dexes:** internal HyperCore transfers, seconds, no bridge.
- **Lighter:** a fast route through Arbitrum and a slower secure route through Ethereum L1. During Arbitrum congestion the fast route has failed and L1 withdrawals took up to about four hours, so rebalance planning assumes hours for Lighter.
- **Aster:** withdrawals by API exist but each needs the main wallet's signature, so the bot only prepares them. The deposit and withdrawal chain is chosen from Aster's asset list and pinned in `docs/venues/aster.md`.
- **Planner:** `spread-ctl rebalance plan` prints every required transfer (from, to, asset, amount, expected time and fee) and, where the venue supports it, an unsigned payload for the operator to sign on a hardware wallet.

### Host placement

- **Primary host: Tokyo.** AWS ap-northeast-1, or bare metal peered into it at about 2 ms. Hyperliquid's validators all sit in ap-northeast-1; measured API latency is about 3 ms from Tokyo against about 243 ms from Frankfurt ([Zenlayer](https://cloud.zenlayer.com/blog/crypto-trading-latency-tokyo), [Glassnode](https://latency.glassnode.com/hyperliquid/fill-latency)).
- **Server-side time dominates on Hyperliquid.** Glassnode measured a median 884 ms for `POST /exchange` from AWS Tokyo in March 2026, almost all of it processing. S1 and S2 do not care; S4 treats it as the latency floor.
- **Lighter and Aster regions are unknown.** `latency-probe` decides whether a second edge process elsewhere is worth it (M5 only).
- **Hyperliquid non-validator node (optional, M5).** Faster market-data reads in Tokyo; needs Ubuntu 24.04, 16 cores, 128 GB RAM, 500 GB SSD, ports 4001–4002 open ([node repo](https://github.com/hyperliquid-dex/node)). Orders still go through the public API.

### `latency-probe` binary

Run it from every candidate host before choosing one. Per venue it measures DNS, TCP connect, TLS handshake, WebSocket ping RTT, REST time-to-first-byte and, only with `--allow-test-orders`, order-ack RTT using a minimum-size post-only order priced 20% away from mark that is cancelled at once. It prints p50 / p90 / p99 and writes JSON to `data/latency/<host>-<date>.json`.

### Clocks and timestamps

The host runs chrony. Every market-data event stores the venue timestamp, the local wall-clock receive time and a monotonic receive time, so latency and staleness are always measurable.

## Architecture

One tokio process with a strict one-way flow: adapters publish normalised market data, strategies emit intents, the risk gate approves or vetoes them, and only the execution engine sends orders to venues.

&#91;embedded content: spread-hunter architecture · one engine loop, one risk gate\]

Strategies never call venues; the risk gate also takes the reconciled book from the reconciler and operator commands from `spread-ctl`.

### Workspace crates

| Crate | Responsibility |
| --- | --- |
| `sh-core` | Newtypes `Px`, `Qty`, `Usd`, `Bps`; venue key `(exchange, dex)`; canonical underlying IDs; collateral asset enum; `Clock` trait; shared errors |
| `sh-venue-api` | `MarketData` and `Execution` traits; normalised events: BookDelta, Trade, Funding, Mark, OrderUpdate, Fill, PositionSnapshot, MetadataChange |
| `sh-venue-hyperliquid` | REST and WS for core and every HIP-3 dex as separate venue instances sharing one connection pool and one signer; L1 action signing with an agent wallet; nonce manager; dex discovery; internal transfers between dexes |
| `sh-venue-lighter` | REST and WS, FFI to the official `lighter-go` signer, account tier handling, nonces per API key |
| `sh-venue-aster` | v3 REST and WS, EIP-712 API-wallet signing, microsecond nonces, exchange and funding info |
| `sh-md` | Order books with sequence-gap detection and snapshot resync, BBO, depth walk, staleness |
| `sh-signals` | Funding normalisation, EMA and realised estimators, cost model, s\_net and ROC |
| `sh-strategy` | `Strategy` trait, S1–S5 |
| `sh-risk` | Pre-trade checks, continuous monitors, kill switch |
| `sh-exec` | Child-order slicing, leg sequencing, hedge timeouts, unwind, order state machine |
| `sh-portfolio` | Positions and balances per venue and collateral asset, reconciliation, PnL attribution, rebalance planner |
| `sh-store` | SQLite (WAL) for state and ledger, raw recorder, Parquet derivation (M1) |
| `sh-sim` | Simulated venue, fill models, replay driver |
| `sh-obs` | Tracing setup, Prometheus metrics, Telegram alerts |

Binaries: `spread-hunter` (daemon, all run modes), `spread-ctl` (control CLI over a Unix socket), `latency-probe`, `backtest`.

### Concurrency model

- One tokio task per venue connection: public market data, private account stream, order sender. HIP-3 dexes share Hyperliquid's sockets where the API allows it. Normalised events flow over a bounded `mpsc` channel into a single-threaded engine loop that owns all mutable state, so strategy, risk and execution need no locks.
- Back-pressure: when the channel is full, an adapter may drop book deltas, mark the book dirty and resync from a snapshot. It never drops order, fill, position or metadata events.
- Internal budget: event receipt to order send under 1 ms p99 on the production host, measured and exported. Robustness comes before speed.

### Approved dependencies

tokio, tokio-tungstenite and reqwest (both with rustls), serde, serde\_json, rust\_decimal, figment, clap, chrono, thiserror, anyhow, tracing, tracing-subscriber, metrics, metrics-exporter-prometheus, sqlx (SQLite), parquet and arrow, zstd, secrecy, zeroize, alloy (Hyperliquid and Aster EIP-712 signing), libloading (Lighter signer), governor (rate limits), proptest, futures-util (WebSocket stream/sink traits), tokio-rustls, rustls and rustls-native-certs (TLS setup and the TLS handshake timing in `latency-probe`). Anything else needs approval.

The official `hyperliquid-rust-sdk`, Aster's published SDKs and community Lighter crates are references for message formats only, not dependencies: signing keys may only touch code reviewed in this repository.

## Signal and execution logic

An opportunity is tradable only after its full round-trip cost at the intended size is subtracted; execution then keeps unhedged exposure under a hard dollar cap at every moment.

### Edge at size

```latex
s_{net} = s_{ema} - \frac{(F_{in} + F_{out} + S_{in} + S_{out}) \cdot 8760}{N \cdot H_{exp}}
```

F are fees on both legs at the account's current tier, S the slippage from walking each book to size N, and H\_exp the expected holding time in hours. H\_exp defaults to 168 and is replaced by the pair's historical median spread lifetime once M1 data exists.

### Sizing

N = min of: capital allowed for the pair; 1% of the smaller venue's open interest; 25% of the depth within 10 bps of mid on the thinner side; the remaining room under the per-venue exposure cap. Base quantity is rounded down to a multiple of both venues' lot steps; the residual is logged, never forced.

### Entry sequence (S1 with the S4 tactic)

1. Pick the maker venue: the one with the lower maker fee, unless its book is too thin for the child size.
2. Slice N into child orders of at most `child_usd` (default $5k).
3. Post a post-only child on the maker venue, priced off the hedge venue's BBO minus hedge taker fee, expected slippage and `edge_bps`. Reprice when the hedge BBO moves more than `reprice_bps`.
4. On every fill, partial or full, send an IOC on the hedge venue for exactly the filled quantity, limited to BBO ± `max_slip_bps`.
5. If the hedge is not complete within `hedge_timeout_ms` (default 1500 for S1), retry up to `hedge_retries` with a wider limit. If quantity is still unhedged, close it on the maker venue with a reduce-only IOC.
6. Total unhedged notional across the book must stay below `max_unhedged_usd` (default $2k). Above it, no new child orders are sent until it is back under.

Exit runs the same sequence in reverse; every exit order is reduce-only.

### Position lifecycle

| State | Entered when | Next states |
| --- | --- | --- |
| Idle | Pair passes all filters | Entering |
| Entering | First child order sent | Hedged, Unwinding |
| Hedged | Both legs within `delta_tolerance` (default 0.5% of N) | Rebalancing, Exiting |
| Rebalancing | Margin ratio on one leg breaches `rebalance_margin_ratio` | Hedged, Exiting |
| Exiting | Exit signal or risk trigger | Closed, Unwinding |
| Unwinding | Hedge failed or venue degraded | Closed, Halted |
| Halted | Reconciliation mismatch or kill switch | Idle (operator only) |
| Closed | Both legs flat and reconciled | Idle |

### Order handling rules

- **Idempotent IDs.** Every order carries a client ID (Hyperliquid `cloid`, Lighter client order index, Aster `newClientOrderId`). On timeout the order goes to `Unknown` and is resolved by querying that ID; it is never resent blindly.
- **Order states.** New, Sent, Acked, PartiallyFilled, Filled, or Cancelled, Rejected, Expired, Unknown. Only transitions confirmed by venue events or REST queries are applied.
- **Nonces.** Hyperliquid needs strictly increasing nonces per signer: millisecond timestamp plus an atomic counter, shared by core and all HIP-3 dexes. Lighter nonces are per API key and come from its signer flow. Aster takes a nonce in microseconds on every signed request.
- **Rate limits.** Each adapter owns a token bucket per endpoint class. All Hyperliquid venue instances (core and HIP-3) share one bucket, because the limits apply to the address and IP; exact weights are pinned in `docs/venues/hyperliquid.md`. Strategies cannot exceed limits; they get back-pressure.
- **S2 legs.** Both legs go through the same Hyperliquid signer. Send them as consecutive actions, or as one batched action if the API accepts orders for different dexes in one request (verify first); never assume the two legs are atomic.
- **Funding timing.** The engine knows each venue's next settlement time and does not open a position whose expected hold is shorter than the time to the next payment on the receiving leg.

## Risk management

A delta-neutral book still dies leg by leg: each venue liquidates on its own mark, so risk is checked per leg, per venue, per platform and for the whole book, before every order and continuously. The risk crate can veto any intent and owns the kill switch.

| Limit | Default | Checked | Action on breach |
| --- | --- | --- | --- |
| `max_platform_share` | Hyperliquid (core + all HIP-3) 60%; Lighter 40%; Aster 40% of equity | Pre-trade | Reject new entries on that platform |
| `max_s2_share` | 50% of equity | Pre-trade | Reject new S2 entries |
| `max_pair_share` | 25% of equity | Pre-trade | Reject or shrink the intent |
| `max_leverage_leg` | 3x; 2x on Lighter legs | Pre-trade | Reject the order |
| `min_liq_distance` | Larger of 25% from mark or 3× the 30-day average daily range | Every 5 s | Rebalance alert, then reduce the position |
| `max_unhedged_usd` | $2,000 | Continuous | Stop new child orders |
| `max_net_delta_usd` | $1,000 book-wide | Continuous | Hedge or unwind the excess |
| `max_in_transit_usd` | 25% of equity | Pre-transfer | Planner will not propose more transfers |
| `daily_loss_limit` | 1% of equity | Continuous | No new entries for 24 h, alert |
| `max_drawdown` | 3% from peak equity | Continuous | All positions to Exiting, alert |
| `md_stale_ms` | 2,000 ms | Continuous | Venue degraded: no new orders there |
| `reconcile_every_s` | 10 s | Loop | Any mismatch: Halted |
| `min_oi_usd` | $5M per venue | Pair filter | Pair not eligible |
| `min_listing_age_h` | 72 h | Pair filter | Pair not eligible unless allowlisted |
| `max_stable_depeg` | 0.3% from USDC for USDT, USDH, USDE | Continuous | No new entries using that collateral, alert |

### Kill switch

Three triggers do the same thing: `spread-ctl kill`, creating the file `run/KILL`, or an authenticated Telegram command. It cancels every open order and blocks new ones. `spread-ctl kill --flatten` also closes all positions with reduce-only IOC slices of at most `child_usd`, hedge-aware so that delta never grows while flattening.

### Venue and systemic risks the code must handle

- **Mark divergence.** Legs are liquidated on different marks and oracles. Margin is monitored per leg, never only net. On S2 legs, an underlying's session change (weekend, market open) is the riskiest moment.
- **Auto-deleveraging.** A position change not caused by our own fills is treated as ADL or liquidation: the other leg is reduced at once and the operator is alerted.
- **HIP-3 deployer actions.** Deployers can change leverage or OI caps, delist, or settle a market at mark (Kinetiq announced settlement of TLT, IWM, USTECH and US500 this week). Metadata is polled; any such change moves affected pairs to Exiting.
- **Outages and stuck withdrawals.** Stale feeds or failing REST mark a venue degraded; open positions stay and an alert fires. Hyperliquid had a 37-minute API outage in July 2025. Lighter withdrawals have stalled for hours during Arbitrum congestion, so Lighter legs carry lower leverage and the planner never relies on a Lighter withdrawal for urgent margin.
- **Thin-market manipulation.** Long-tail assets on Hyperliquid have been squeezed on purpose (JELLY, POPCAT). The OI, listing-age and share-of-OI filters exist for this.
- **Stablecoin basis.** Aster legs margin in the contract's `marginAsset` (USDT, USDC, USD1 or U), some HIP-3 dexes margin in USDH or USDE, and USDH is being retired in favour of USDC. Each collateral asset's rate to USDC is a PnL line and a risk input.
- **Custody and contracts.** Exposure caps per platform and an idle reserve in self-custody rather than on any venue. Funds in bridges are tracked as a separate balance until they land.

## Config, secrets, deployment, operations

The server holds only keys that can trade, never keys that can withdraw; everything else follows from that rule.

### Configuration

`config/default.toml` plus an environment overlay such as `config/live.toml`, loaded with `figment` and validated on start (unknown keys are an error). Strategy thresholds can be hot-reloaded through `spread-ctl reload`; venue and credential settings need a restart. Canonical symbol mapping lives in `config/symbols.toml`.

```toml
[engine]
mode = "paper"            # record | scan | paper | live
equity_reserve_pct = 15

[venues.hyperliquid]
trading_enabled = false   # оператор включает после теста на малом объёме
rest = "https://api.hyperliquid.xyz"
dexes = "auto"           # core + все HIP-3 дексы из метаданных
dex_denylist = []

[venues.lighter]
trading_enabled = false
account_tier = "standard"  # standard | premium
signer_lib = "signers/lighter-signer-linux-amd64.so"

[venues.aster]
trading_enabled = false
rest = "https://fapi.asterdex.com"

[strategy.s1_funding_carry]
min_gross_apr = 15.0
min_net_apr = 12.0
exit_apr = 3.0
max_breakeven_hours = 72
k_settlements = 6
child_usd = 5000
hedge_timeout_ms = 1500

[strategy.s2_cross_dex]
min_net_apr = 10.0
session_guard_min = 60

[risk]
max_platform_share_pct = { hyperliquid = 60, lighter = 40, aster = 40 }
max_s2_share_pct = 50
max_leverage_leg = 3.0
max_leverage_lighter = 2.0
max_unhedged_usd = 2000
daily_loss_limit_pct = 1.0
max_drawdown_pct = 3.0
```

### Keys and secrets

- **Hyperliquid:** trade through an API (agent) wallet approved by the master account. Agent wallets can place orders but cannot withdraw. The master key stays on a hardware wallet.
- **Lighter:** a dedicated API key index, separate from the L1 wallet key, which never reaches the server. Check in Lighter's docs which transaction types an API key can sign; if it can sign withdrawals or transfers, cap the Lighter balance and alert on any outgoing transfer the operator did not start.
- **Aster:** an API wallet (signer) authorised by the main wallet. API withdrawals also need the main wallet's signature, so the signer alone cannot move funds out; confirm the signer's permissions and expiry and pin them in `docs/venues/aster.md`.
- **Storage:** one `age`-encrypted secrets file, decrypted at start through systemd `LoadCredentialEncrypted` or a passphrase prompt. In memory, secrets live in `secrecy::SecretString` and are zeroised on drop. `Debug` and `Display` for any type holding a secret print `[redacted]`.

### Build and deployment

- Development on macOS arm64 and Debian; production target `x86_64-unknown-linux-gnu` on Ubuntu 24.04 LTS in Tokyo. The Lighter signer library is loaded per platform (`linux-amd64.so` on the server, `darwin-arm64.dylib` for local dev).
- Release profile: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`. A panic kills the process; systemd restarts it; the startup sequence below guarantees nothing trades before reconciliation.
- systemd unit with `Restart=on-failure`, `NoNewPrivileges`, `ProtectSystem=strict`, `PrivateTmp` and `ReadWritePaths` limited to `data/` and `run/`. Dedicated non-root user.
- Firewall denies all inbound except SSH (keys only). Metrics, Grafana and the control socket are reachable only over WireGuard from the operator's machine.

### Startup and shutdown

1. Load and validate config, then secrets.
2. Connect market data; fetch metadata for every venue and every HIP-3 dex (fees, steps, intervals, leverage caps, margin modes, collateral).
3. Reconcile positions, open orders and balances against every venue.
4. Run risk checks on the reconciled book.
5. Enable strategies. Any failure in steps 2–4 leaves the engine in Halted with an alert.

On SIGTERM: stop new orders, cancel resting maker orders, keep positions, flush state, exit. SQLite state is backed up hourly to an encrypted off-host copy.

## Simulation: recorder, scanner, backtest, paper trading

The same strategy and risk code runs in all modes; only the venue implementation and the clock change. Strategy code never reads the wall clock directly: it gets a `Clock` trait, so replays are deterministic.

### Recorder (M0)

- Streams per venue: L2 book (top 20 levels), trades, BBO, predicted and settled funding, mark / index / oracle prices, open interest, metadata snapshots every 5 minutes.
- Raw venue messages are stored exactly as received, with local receive timestamps, in zstd-compressed JSONL files split per venue and per UTC hour. This allows bit-exact replay through the adapters.
- A nightly job derives Parquet tables (1-second BBO, funding history, book snapshots, OI) for analysis in Polars or Python. Deferred to M1: M0 delivers the raw recordings only.

### Scanner (M1): does 10–20% exist at $100k?

- Every minute, for every eligible pair: s now, s EMA 24 h, s realised 7 d, round-trip cost at $10k, $50k and $100k per leg, break-even hours, s\_net and ROC.
- A shadow portfolio allocates $100k by the S1 and S2 rules and the risk limits, books funding at actual settlement times and costs from the recorded books, and reports what it would have earned.
- Daily report as Markdown plus CSV in `reports/`, and a short Telegram summary. Acceptance for M1 is a 30-day shadow-portfolio report; live trading is decided on that number.

### Backtester

- Event-driven replay of raw recordings through the real adapters' parsers, the strategy and the risk crate.
- Taker fills walk the recorded book at event time plus a per-venue latency: measured RTT from `latency-probe` plus protocol delays (for example Lighter Standard: 300 ms taker, 0 ms maker, 300 ms cancel and modify).
- Maker fills are conservative: a resting order fills only when recorded trades print through its price with cumulative volume at least equal to the estimated queue ahead of it.
- Funding is applied at each venue's settlement times from recorded settled rates.
- Same seed and same data give byte-identical results.

### Paper and dry-run

- `paper`: live data, simulated fills with the backtester's model, full persistence and alerts. Required for at least 14 days before `live`.
- `live --dry-run`: real credentials and metadata; orders are built and signed but not sent, and are logged instead. This validates signing and serialisation.
- Integration tests against Hyperliquid testnet, Lighter testnet (chain ID 300) and Aster testnet cover the full order lifecycle: post, partial fill, cancel, reduce-only, query by client ID.

## Observability

The operator must be able to answer three questions at any moment from the dashboard: am I hedged, how far is every leg from liquidation, and where did today's PnL come from.

### Logs

`tracing` with JSON output to daily-rotated files. One span per intent and per order, carrying the client order ID, venue and pair. Log messages in English; secrets never appear (see Config).

### Metrics

Prometheus exporter on `127.0.0.1:9464`, Grafana dashboard JSON committed in `ops/grafana/`.

| Metric | Labels | Type |
| --- | --- | --- |
| `md_staleness_ms` | venue | gauge |
| `ws_reconnects_total` | venue | counter |
| `rtt_ms` | venue, endpoint | histogram |
| `order_ack_ms` | venue | histogram |
| `orders_total` | venue, status | counter |
| `unhedged_usd`, `net_delta_usd` | none | gauge |
| `margin_ratio` | venue | gauge |
| `liq_distance_pct` | venue, pair | gauge |
| `equity_usd` | venue | gauge |
| `funding_accrued_usd` | venue, pair | counter |
| `fees_usd` | venue | counter |
| `slippage_bps` | venue | histogram |
| `s_net_apr` | pair | gauge |

### Alerts

Telegram bot running on the server, three levels with de-duplication and rate limiting. CRIT: kill switch fired, Halted, reconciliation mismatch, liquidation distance under limit, venue degraded with open positions, daily loss limit hit. WARN: rebalance needed, funding sign flip on a held pair, collateral depeg threshold, metadata change on a held pair. INFO: daily report.

### PnL attribution

Per position and per day, split into: funding received and paid, trading fees, slippage against the decision mid, price PnL (should stay near zero), collateral basis (USDT, USD1, U, USDH, USDE against USDC). Points and airdrop accruals are tracked as quantities only, never as USD PnL.

### Accounting export

`spread-ctl export --year 2026` writes a CSV ledger of every fill, funding payment, fee and transfer in USD, with optional conversion into a configurable reporting currency at a configurable daily reference rate. This is input for the operator's own accounting; the engine does not compute tax.

## Conventions, milestones, acceptance

No milestone starts before the previous one meets its acceptance criteria; M1's 30-day shadow-portfolio result decides whether live trading happens at all.

### Code conventions

- Rust stable, edition 2024, toolchain pinned in `rust-toolchain.toml`. `rustfmt` and `cargo clippy -- -D warnings` must pass.
- `#![forbid(unsafe_code)]` in every crate except the isolated Lighter signer FFI module, which documents each `unsafe` block.
- **Comments in Russian.** Every public item gets a Russian doc comment; inline comments in Russian explain why, not what. Identifiers and log messages stay in English.
- Money types: `rust_decimal::Decimal` behind newtypes `Px`, `Qty`, `Usd`, `Bps`. `f64` only in analytics and metrics, never in order paths.
- Errors: `thiserror` in libraries, `anyhow` only in binaries. No `unwrap` or `expect` outside tests and startup config loading.
- Tests: unit tests for all math, `proptest` for order books and lot rounding, JSON fixture tests per adapter, testnet integration behind a `testnet` feature, replay regression tests.
- CI (GitHub Actions): fmt, clippy, test, `cargo-deny` for licences and advisories, `cargo-audit`.

Example of the expected comment style:

```rust
/// Нормализует ставку фандинга за интервал к годовой (APR).
/// Интервал берём из метаданных площадки, а не хардкодим.
pub fn funding_apr(rate_per_interval: Decimal, interval_hours: Decimal) -> Decimal {
    // 8760 часов в году; високосные годы намеренно не учитываем
    rate_per_interval * dec!(8760) / interval_hours
}
```

### Testing responsibility

From M1 on, Claude Code writes the code for M1–M4 and its unit, property, fixture and simulated-venue tests, but runs **no acceptance tests and no live or testnet tests**: no recording runs counted toward acceptance, no orders, no `selftest`, no shadow or paper runs against live venues. The operator runs all of them on his own server, including every acceptance criterion in the table below. Live trading is off by default: `engine.mode` defaults to `paper`, and live trading requires `mode = "live"` in the config together with the CLI flags `--live --i-understand-real-money`; every venue keeps `trading_enabled = false` by default.

### Milestones

| Milestone | Deliverables | Acceptance |
| --- | --- | --- |
| M0 | Workspace skeleton; market-data adapters for Hyperliquid core + HIP-3 dexes, Lighter, Aster; recorder; `latency-probe` | 72 h continuous recording from all venue families with under 0.1% gap time; latency report from at least 2 candidate hosts |
| M1 | Parquet derivation of the M0 recordings; scanner for S1 and S2 pairs, shadow portfolio, daily reports | 30-day report; go to M2 only if shadow ROC ≥ 10% annualised with shadow drawdown under 2% |
| M2 | Execution engine, risk crate, S5 maker-first tactic, paper mode for S1 and S2 | 14 days paper with zero unhandled order states; kill switch, venue-outage and HIP-3 delisting drills pass |
| M3 | Live with $5k total on Hyperliquid (core + HIP-3) and Lighter, max 2 pairs | 14 days live; realised slippage within 30% of the paper model; no open CRIT incidents |
| M4 | Aster live; capital ramp $25k → $50k → $100k; rebalance planner with unsigned payloads | Each step after 14 clean days; ROC and drawdown tracked against M1 |
| M5 | S3 spot–perp, S4 convergence, optional Hyperliquid non-validator node, optional second edge host | Backtest and paper show positive net edge after measured latency |
| M6 | S6 Boros rate lock (optional); evaluate backlog items | Every S6 lock needs CLI approval; each backlog item gets its own spec addendum |

## Sources

As of 2026-10-29: [arb.sh perp DEX tracker](https://arb.sh/) · [HIP-3 builder-deployed perps](https://hyperliquid.gitbook.io/Hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals) · [Hyperliquid vaults for leaders](https://hyperliquid.gitbook.io/hyperliquid-docs/vaults/for-vault-leaders) · [Hyperliquid withdraw reference](https://docs.chainstack.com/reference/hyperliquid-exchange-withdraw) · [Hyperliquid node](https://github.com/hyperliquid-dex/node) · [Lighter trading fees](https://docs.lighter.xyz/trading) · [Lighter environments](https://apidocs.lighter.xyz/docs/environments) · [lighter-sdk crate](https://docs.rs/lighter-sdk) · [Aster API v3](https://asterdex.github.io/aster-api-website/futures-v3/general-info/) · [Glassnode Hyperliquid fill latency](https://latency.glassnode.com/hyperliquid/fill-latency) · [Zenlayer Tokyo latency](https://cloud.zenlayer.com/blog/crypto-trading-latency-tokyo) · [Pendle Boros docs](https://docs.pendle.finance/boros-docs/Introduction)

## Revisions

- 2026-11-05: Testing responsibility section added (the operator runs every acceptance, testnet and live test; Claude Code only writes code and offline tests). Live trading gate: `engine.mode = "live"` plus `--live --i-understand-real-money`; `engine.mode` defaults to `paper`.
- 2026-10-31: Lighter Standard/Premium delays updated to the current Lighter docs (Standard has no maker delay; takers and cancels/modifies wait 300 ms). Aster collateral is per contract (USDT, USDC, USD1, U) instead of "USDT-quoted", as seen in live `exchangeInfo`. futures-util, tokio-rustls, rustls and rustls-native-certs added to the approved dependencies. Parquet derivation moved from M0 to M1. Details and sources are in `docs/venues/`.
