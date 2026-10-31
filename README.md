# spread-hunter

Демон на Rust для дельта-нейтральных спредов по фандингу и цене между perp-DEX.
Полная спецификация — в [SPEC.md](SPEC.md); заметки по API площадок — в
[docs/venues/](docs/venues/).

**Текущий этап: M0** — только запись рыночных данных. В репозитории нет подписей,
приватных эндпоинтов и кода ордеров.

## Что входит в M0

| Крейт | Содержимое |
| --- | --- |
| `sh-core` | Десятичные новые типы `Px`/`Qty`/`Usd`/`Bps`, `VenueKey (exchange, dex)`, перечисление обеспечения, `Clock` (настенное и монотонное время), типы сырых записей и системных событий |
| `sh-venue-api` | Трейт `MarketDataVenue`, нормализованные события, метаданные инструментов, вывод интервала фандинга, общий драйвер WebSocket (переподключение с задержкой, пинги, тайм-аут тишины, писатель с лимитом сообщений), REST-клиент с весовым лимитером, планировщик подписок |
| `sh-venue-hyperliquid` | Ядро и все HIP-3 дексы (обнаружение через `perpDexs` при старте и каждый час) |
| `sh-venue-lighter` | WebSocket только для чтения, стаканы с проверкой `nonce`, переподписка при разрыве |
| `sh-venue-aster` | Комбинированные потоки futures v3, стаканы по дельтам с синхронизацией через REST-снимок |
| `sh-md` | Стакан, обнаружение разрывов последовательности с буферизацией и пересинхронизацией по снимку, BBO, проход по глубине, устаревание |
| `sh-store` | Сырой рекордер (zstd JSONL по площадке и часу UTC), чтение, отчёт о разрывах |
| `sh-obs` | JSON-логи (stdout и ежедневный файл), экспорт Prometheus на `127.0.0.1:9464` |

Бинарники: `spread-hunter` (`run`, `metadata`, `inspect`, `dump`) и `latency-probe`.

## Сборка и проверки

```bash
cargo build --release
```

```bash
cargo fmt --all -- --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

Версия тулчейна закреплена в `rust-toolchain.toml`.

## Рекордер

```bash
./target/release/spread-hunter --config config/default.toml run
```

Файлы: `data/raw/<venue>/<YYYY-MM-DD>/<venue>-<YYYY-MM-DD>T<HH>.jsonl.zst`.
Каждый файл — последовательность независимых кадров zstd (сбрасываются раз в
секунду), поэтому при аварии теряется не больше `flush_interval_ms`, а уже
записанные кадры читаются. Каждая строка — один объект JSON:

- заголовок (первая строка после каждого открытия файла): `{"hdr":{"format":"sh-raw-v1","session":...,"host":...,"session_start_wall_ns":...,"version":...}}`
- запись: `{"exchange":"lighter","src":"ws|rest|sys|out","ep":"<REST-запрос>","conn":"lt-ws-0","recv":{"wall_ns":...,"mono_ns":...},"payload":"<сообщение ровно как пришло>"}`

`payload` — сообщение площадки байт в байт. `mono_ns` — монотонное время от старта
сессии, сравнимо только внутри одной `session`. `src=out` — отправленные нами
сообщения подписки; `src=sys` — события соединений и синхронизации стаканов
(`conn_up`, `conn_down`, `book_gap`, `book_synced`, ...).

Проверка и распаковка:

```bash
./target/release/spread-hunter inspect data/raw
```

```bash
./target/release/spread-hunter dump data/raw/lighter/2026-10-29/lighter-2026-10-29T06.jsonl.zst | head
```

`inspect` показывает по каждой площадке время без соединения и время
рассинхронизации стаканов как долю записанного периода (критерий приёмки M0 —
меньше 0,1%), а также самую длинную паузу между записями (простой рекордера).

Объём на диске с конфигом по умолчанию (замер 2026-10-31): около 0,94 ГБ в час в
сжатом виде (Aster ~0,66 ГБ/ч, Lighter ~0,17 ГБ/ч, Hyperliquid ~0,10 ГБ/ч), то есть
около 70 ГБ на 72-часовой прогон приёмки. Общий поток BBO Aster `!bookTicker`
выключен по умолчанию (`venues.aster.subscribe_book_ticker = false`); с ним Aster
пишет ~1,05 ГБ/ч, а всего выходит ~1,4 ГБ/ч.

## Замер задержек

```bash
./target/release/latency-probe --samples 50
```

Измеряет по каждой площадке DNS, TCP connect, TLS handshake, RTT пинга WebSocket и
время до первого байта REST, печатает p50/p90/p99 и пишет
`data/latency/<host>-<date>.json`. Замер подтверждения ордера
(`--allow-test-orders`) в M0 не входит. Если настроен HTTPS-прокси, результат
помечается `via_proxy`, и цифры TCP/TLS относятся к пути через прокси.

## Запуск рекордера на Ubuntu 24.04

```bash
sudo apt-get install -y build-essential cmake chrony
```

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
```

```bash
cargo build --release
```

```bash
sudo useradd --system --home /opt/spread-hunter --shell /usr/sbin/nologin spreadhunter
```

```bash
sudo mkdir -p /opt/spread-hunter/{bin,config,data,run}
```

```bash
sudo cp target/release/spread-hunter target/release/latency-probe /opt/spread-hunter/bin/
```

```bash
sudo cp config/default.toml /opt/spread-hunter/config/
```

```bash
sudo chown -R spreadhunter:spreadhunter /opt/spread-hunter/data /opt/spread-hunter/run
```

```bash
sudo cp ops/systemd/spread-hunter-recorder.service /etc/systemd/system/
```

```bash
sudo systemctl daemon-reload
```

```bash
sudo systemctl enable --now spread-hunter-recorder
```

```bash
journalctl -u spread-hunter-recorder -f
```

Пути в `config/default.toml` относительные (`data/raw`, `data/logs`); unit-файл
задаёт `WorkingDirectory=/opt/spread-hunter`, поэтому они указывают в
`/opt/spread-hunter/data`. Метрики:

```bash
curl -s 127.0.0.1:9464/metrics
```

Нужен исходящий доступ к `api.hyperliquid.xyz`, `mainnet.zklighter.elliot.ai`,
`fapi.asterdex.com`, `fstream.asterdex.com` (TCP 443). Lighter отклоняет
рукопожатие WebSocket из ограниченных юрисдикций без `?readonly=true`; URL по
умолчанию уже его содержит (для M0 нужен только режим чтения).
