# spread-hunter

Демон на Rust для дельта-нейтральных спредов по фандингу и цене между perp-DEX.
Полная спецификация — в [SPEC.md](SPEC.md); заметки по API площадок — в
[docs/venues/](docs/venues/).

**Текущий этап** — см. [PROGRESS.md](PROGRESS.md). Готовы M0 (запись рыночных данных),
M1 (сканер, теневой портфель, отчёты, Parquet) и M2 (движок исполнения, риск, S5, paper,
backtest, `spread-ctl`, приватные адаптеры, `selftest`). Режим по умолчанию — `paper`:
реальных ордеров нет. Живая торговля — с M3 и только при явном включении оператором
(`engine.mode = "live"` и флаги `--live --i-understand-real-money`).

Claude Code пишет код и офлайн-тесты; приёмку, testnet и live оператор запускает сам
(SPEC.md, «Testing responsibility»).

## Крейты

| Крейт | Содержимое |
| --- | --- |
| `sh-core` | Десятичные новые типы `Px`/`Qty`/`Usd`/`Bps`, `VenueKey (exchange, dex)`, перечисление обеспечения, `Clock` (настенное и монотонное время), типы сырых записей и системных событий |
| `sh-venue-api` | Трейт `MarketDataVenue`, нормализованные события, метаданные инструментов, вывод интервала фандинга, общий драйвер WebSocket (переподключение с задержкой, пинги, тайм-аут тишины, писатель с лимитом сообщений), REST-клиент с весовым лимитером, планировщик подписок |
| `sh-venue-hyperliquid` | Ядро и все HIP-3 дексы (обнаружение через `perpDexs` при старте и каждый час) |
| `sh-venue-lighter` | WebSocket только для чтения, стаканы с проверкой `nonce`, переподписка при разрыве |
| `sh-venue-aster` | Комбинированные потоки futures v3, стаканы по дельтам с синхронизацией через REST-снимок |
| `sh-md` | Стакан, обнаружение разрывов последовательности с буферизацией и пересинхронизацией по снимку, BBO, проход по глубине, устаревание |
| `sh-store` | Сырой рекордер (zstd JSONL по площадке и часу UTC), чтение, отчёт о разрывах, SQLite (WAL), таблицы Parquet |
| `sh-obs` | JSON-логи (stdout и ежедневный файл), экспорт Prometheus на `127.0.0.1:9464`, оповещения Telegram |
| `sh-signals` | APR фандинга, оценки спреда, модель издержек, `s_net`, ROC, точка безубыточности, размер позиции, сопоставление символов, календари сессий, оценка пар |
| `sh-portfolio` | Теневой портфель M1, дневные отчёты, книга позиций M2 (жизненный цикл пар, сверка, атрибуция PnL) |
| `sh-sim` | Симулятор площадок: тейкер по стакану с задержкой, мейкер по очереди, фандинг, режимы отказа |
| `sh-exec` | Автомат состояний ордера, задача пары S5 (мейкер-первый вход, хедж, разворот) |
| `sh-risk` | Проверки перед входом и ордером, мониторы, kill switch |
| `sh-strategy` | Трейт `Strategy`, S1 и S2 |

Бинарники: `spread-hunter` (`run`, `scan --replay`, `derive`, `selftest`, `metadata`,
`inspect`, `dump`), `spread-ctl`, `backtest` и `latency-probe`. Подробности M1 —
[docs/scanner.md](docs/scanner.md), M2 — [docs/execution.md](docs/execution.md).

## Режимы

`engine.mode` в конфиге: `record` — только запись; `scan` — запись, сканер раз в минуту,
теневой портфель на $100k, дневные отчёты в `reports/`; `paper` (по умолчанию) — сканер,
стратегии S1/S2, риск и исполнение S5 на симуляторе с живыми данными, состояние в SQLite,
`spread-ctl` и kill switch. Сырые записи в любом режиме пишутся при `recorder.enabled = true`
(по умолчанию). `trading_enabled` у всех площадок `false`; в M2 он не нужен (paper не торгует).

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

## Запуск

```bash
./target/release/spread-hunter --config config/default.toml run
```

Повтор сканера по записям (без сети) и производные Parquet:

```bash
./target/release/spread-hunter scan --replay data/raw --from 2026-11-01 --to 2026-11-08
```

```bash
./target/release/spread-hunter derive --date 2026-11-01
```

Бэктест, управление и проверка приватных адаптеров (selftest ставит реальные малые
ордера — запускает только оператор, см. [docs/execution.md](docs/execution.md)):

```bash
./target/release/backtest --replay data/raw --from 2026-11-01 --to 2026-11-08
```

```bash
./target/release/spread-ctl status
```

```bash
./target/release/spread-hunter selftest --network testnet --max-notional-usd 20
```

## Рекордер

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

## Установка на Ubuntu 24.04

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
sudo mkdir -p /opt/spread-hunter/{bin,config,data,run,reports}
```

```bash
sudo cp target/release/spread-hunter target/release/spread-ctl target/release/backtest target/release/latency-probe /opt/spread-hunter/bin/
```

```bash
sudo cp config/default.toml config/symbols.toml /opt/spread-hunter/config/
```

```bash
sudo chown -R spreadhunter:spreadhunter /opt/spread-hunter/data /opt/spread-hunter/run /opt/spread-hunter/reports /opt/spread-hunter/config
```

```bash
sudo cp ops/systemd/spread-hunter.service ops/systemd/spread-hunter-derive.service ops/systemd/spread-hunter-derive.timer /etc/systemd/system/
```

```bash
sudo systemctl daemon-reload
```

```bash
sudo systemctl enable --now spread-hunter spread-hunter-derive.timer
```

```bash
journalctl -u spread-hunter -f
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
умолчанию уже его содержит (для рыночных данных нужен только режим чтения).

## Секреты

Ключей в репозитории нет. Секреты — один TOML-файл (пример ниже), зашифрованный на
сервере и расшифровываемый при старте: systemd `LoadCredentialEncrypted` (основной
способ) или внешняя команда, например `age -d`, которая спросит пароль. Для `paper` нужен
только Telegram (по желанию); ключи площадок нужны для `selftest` и live (M3). На сервер
кладутся только ключи, которые могут торговать, но не выводить средства.

```toml
[telegram]
bot_token = "123456:ABC..."
chat_id = "-1001234567890"
allowed_user_ids = [111111111]   # кто может слать /kill и /status

[hyperliquid]
account_address = "0x…"          # основной аккаунт (ключ на аппаратном кошельке)
agent_private_key = "0x…"        # агент-кошелёк, одобренный основным: торгует, не выводит

[lighter]
account_index = 12345
api_key_index = 3                # отдельный индекс API-ключа; ключ L1 сюда не кладётся
api_private_key = "…"

[aster]
user = "0x…"                     # основной кошелёк
signer = "0x…"                   # API-кошелёк
signer_private_key = "0x…"
```

Библиотеку подписанта Lighter (`lighter-signer-linux-amd64.so` из официального
`lighter-go`/Python SDK) положите в `/opt/spread-hunter/signers/` и укажите путь в
`[execution] lighter_signer_lib`.

Шифрование для systemd (файл с открытым текстом после этого удалить):

```bash
sudo systemd-creds encrypt --name=secrets /root/secrets.toml /etc/credstore.encrypted/spread-hunter-secrets
```

```bash
sudo shred -u /root/secrets.toml
```

Затем в `spread-hunter.service` раскомментировать строку `LoadCredentialEncrypted=...`, в
конфиге задать `[secrets] source = "credential:secrets"` и `[telegram] enabled = true`.
