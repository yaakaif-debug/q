# spread-hunter

Rust daemon for delta-neutral funding and price spreads between perp DEXes.
The full specification is in [SPEC.md](SPEC.md); venue API notes are in
[docs/venues/](docs/venues/).

**Current milestone: M0** — market-data recording only. There is no signing,
no private endpoint and no order code anywhere in the repository.

## What M0 contains

| Crate | Contents |
| --- | --- |
| `sh-core` | `Px`/`Qty`/`Usd`/`Bps` decimal newtypes, `VenueKey (exchange, dex)`, collateral enum, `Clock` (wall + monotonic), raw record and sys event types |
| `sh-venue-api` | `MarketDataVenue` trait, normalised events, instrument metadata, funding-interval inference, shared WebSocket driver (reconnect/backoff, pings, idle timeout, rate-limited writer), REST client with weight limiter, subscription planner |
| `sh-venue-hyperliquid` | Core + every HIP-3 dex (discovered via `perpDexs` at startup and hourly) |
| `sh-venue-lighter` | Read-only WebSocket, nonce-checked books, resubscribe on gap |
| `sh-venue-aster` | Futures v3 combined streams, diff-depth books with REST snapshot resync |
| `sh-md` | Order book, sequence-gap detection with buffering and snapshot resync, BBO, depth walk, staleness |
| `sh-store` | Raw recorder (zstd JSONL per venue per UTC hour), reader, gap report |
| `sh-obs` | JSON logs (stdout + daily file), Prometheus exporter on `127.0.0.1:9464` |

Binaries: `spread-hunter` (`run`, `metadata`, `inspect`, `dump`) and `latency-probe`.

## Build and test

```bash
cargo build --release
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The toolchain is pinned in `rust-toolchain.toml`.

## Recorder

```bash
./target/release/spread-hunter --config config/default.toml run
```

Files: `data/raw/<venue>/<YYYY-MM-DD>/<venue>-<YYYY-MM-DD>T<HH>.jsonl.zst`.
Each file is a sequence of independent zstd frames (flushed every second), so
a crash loses at most `flush_interval_ms` of data and earlier frames stay
readable. Every line is one JSON object:

- header (first line after each open): `{"hdr":{"format":"sh-raw-v1","session":...,"host":...,"session_start_wall_ns":...,"version":...}}`
- record: `{"exchange":"lighter","src":"ws|rest|sys|out","ep":"<rest request>","conn":"lt-ws-0","recv":{"wall_ns":...,"mono_ns":...},"payload":"<message exactly as received>"}`

`payload` is the venue message byte-for-byte. `mono_ns` is monotonic time since
the session start and is comparable only within one `session`. `src=out`
records the subscription messages we sent; `src=sys` records connection and
book-sync events (`conn_up`, `conn_down`, `book_gap`, `book_synced`, ...).

Inspect and decompress:

```bash
./target/release/spread-hunter inspect data/raw
./target/release/spread-hunter dump data/raw/lighter/2026-10-29/lighter-2026-10-29T06.jsonl.zst | head
```

`inspect` reports, per venue, connection downtime and book-desync time as a
fraction of the recorded span (the M0 acceptance metric is < 0.1%), plus the
longest silence between records (recorder downtime).

Disk usage measured on 2026-10-29 with the default config: about 1.4 GB per
hour compressed (Aster ~1.05 GB/h, Lighter ~0.24 GB/h, Hyperliquid ~0.1 GB/h),
i.e. about 100 GB for the 72-hour acceptance run. `venues.aster.subscribe_book_ticker = false`
roughly halves the Aster share.

## Latency probe

```bash
./target/release/latency-probe --samples 50
```

Measures DNS, TCP connect, TLS handshake, WebSocket ping RTT and REST
time-to-first-byte per venue, prints p50/p90/p99 and writes
`data/latency/<host>-<date>.json`. Order-ack RTT (`--allow-test-orders`) is not
part of M0. If an HTTPS proxy is configured the result is marked `via_proxy`
and the TCP/TLS numbers describe the proxy path.

## Deploying the recorder on Ubuntu 24.04

```bash
sudo apt-get install -y build-essential cmake chrony
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
cargo build --release
sudo useradd --system --home /opt/spread-hunter --shell /usr/sbin/nologin spreadhunter
sudo mkdir -p /opt/spread-hunter/{bin,config,data,run}
sudo cp target/release/spread-hunter target/release/latency-probe /opt/spread-hunter/bin/
sudo cp config/default.toml /opt/spread-hunter/config/
sudo chown -R spreadhunter:spreadhunter /opt/spread-hunter/data /opt/spread-hunter/run
sudo cp ops/systemd/spread-hunter-recorder.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now spread-hunter-recorder
journalctl -u spread-hunter-recorder -f
```

Paths in `config/default.toml` are relative (`data/raw`, `data/logs`); the unit
sets `WorkingDirectory=/opt/spread-hunter`, so they resolve under
`/opt/spread-hunter/data`. Metrics: `curl -s 127.0.0.1:9464/metrics`.

Outbound access needed: `api.hyperliquid.xyz`, `mainnet.zklighter.elliot.ai`,
`fapi.asterdex.com`, `fstream.asterdex.com` (all TCP 443). Lighter rejects
WebSocket handshakes from restricted jurisdictions unless `?readonly=true` is
used; the default URL already uses it (read-only is all M0 needs).
