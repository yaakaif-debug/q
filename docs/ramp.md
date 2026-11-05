# Aster live, ступени капитала, перебалансировка, учётная выгрузка (M4)

Код M4 написан и проверен только офлайн. Live, selftest и наращивание капитала делает
оператор (SPEC.md, «Testing responsibility»).

## Aster в live

`venues.aster.trading_enabled = true` теперь допустимо (по умолчанию `false`). Порядок тот же,
что для Hyperliquid и Lighter в docs/live.md: selftest Aster на testnet и mainnet малым номиналом,
`live --dry-run`, затем `trading_enabled = true` в `config/live.toml` и перезапуск
`spread-hunter-live`. Секреты — `[aster]` (`user`, `signer`, `signer_private_key`); адаптер
проверяет, что ключ соответствует адресу `signer`. Хосты v3 — `execution.aster_rest`/`aster_ws`
(по умолчанию `fapi3.asterdex.com` / `fstream.asterdex.com`, UNVERIFIED).

## Ступени капитала

`[stage] max_capital_usd` допускает только значения лестницы `5000 → 25000 → 50000 → 100000`
(иначе конфиг не загрузится); `max_pairs` — число пар. Риск не даёт открыть пару сверх ступени,
размер пары ограничен её остатком. Повышает ступень оператор, правкой `config/live.toml` и
перезапуском, после 14 чистых дней на текущей:

```bash
./target/release/spread-ctl --socket /opt/spread-hunter/run/spread-hunter.sock status
```

Поле `clean_days` — полные сутки без CRIT-оповещений (с первого запуска или последнего CRIT);
метрика `clean_days`. ROC и просадку против M1 оператор сравнивает по дневным отчётам и
выгрузке (ниже).

## Перебалансировка: `spread-ctl rebalance plan`

```bash
./target/release/spread-ctl rebalance plan --in-transit-usd 0
```

Планировщик сравнивает капитал каждой площадки (декса для Hyperliquid) с маржой её ног
(номинал / плечо с резервом `equity_reserve_pct`) и предлагает переводы. Он **ничего не
подписывает и не отправляет**: результат — JSON с неподписанными данными для оператора.

- Внутри Hyperliquid (ядро ↔ HIP-3) — действие `agentSendAsset` (nonce подставляется при подписи).
- Между платформами — вывод с источника и депозит на адрес из `[rebalance] deposit_addresses`:
  для Hyperliquid — типизированные данные `withdraw3` для подписи основным кошельком
  (домен и chainId — UNVERIFIED), для Lighter и Aster — описание вывода основным кошельком.
- Сначала срочные дефициты (маржа поддержания / капитал ≥ `rebalance_margin_ratio`), источники —
  та же платформа, затем крупнейший излишек; Lighter — последним и никогда для срочной маржи.
- В пути одновременно не больше `risk.max_in_transit_pct` капитала с учётом `--in-transit-usd`;
  переводы меньше `[rebalance] min_transfer_usd` не предлагаются; на источнике остаётся запас
  `keep_buffer_pct` сверх нужной маржи. Непокрытые дефициты и причины — в `uncovered` и `notes`.

## Учётная выгрузка: `spread-ctl export --year`

```bash
./target/release/spread-ctl export --year 2026 --database data/state/live.sqlite --out reports/ledger-2026.csv
```

С валютой отчёта (файл курсов оператора, `YYYY-MM-DD,rate` — единиц валюты за 1 USD; берётся
курс дня или последний известный раньше):

```bash
./target/release/spread-ctl export --year 2026 --currency EUR --rates rates-eur.csv --out reports/ledger-2026-eur.csv
```

Строки: каждое заполнение (денежный поток: покупка — минус, продажа — плюс), комиссия
заполнения, платёж фандинга, перевод. Теневой портфель сканера и атрибуция (проскальзывание,
ценовой результат) не выгружаются. Выгрузка читает базу напрямую, демон не нужен. Налоги движок
не считает.

## Шаги оператора (Ubuntu 24.04)

1. Обновить бинарники (`spread-hunter`, `spread-ctl`, `backtest`) и конфиги, как в README.
2. В `config/default.toml` (или `live.toml`) заполнить `[rebalance] deposit_addresses` и
   `hyperliquid_account` публичными адресами.
3. Aster: selftest → dry-run → `venues.aster.trading_enabled = true` в `config/live.toml` →
   `sudo systemctl restart spread-hunter-live`.
4. После 14 чистых дней: `[stage] max_capital_usd = 25000` (и нужное `max_pairs`), перезапуск;
   далее 50000 и 100000 по тому же правилу.
5. Раз в день — `spread-ctl rebalance plan`; переводы подписывать вручную аппаратным кошельком.
6. В конце года — `spread-ctl export --year …`.
