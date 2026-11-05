# Прогресс разработки

Этот файл — точка продолжения для новой сессии. Обновляется после каждого этапа.

## Правила работы

- Один этап = одна ветка = один PR. Каждая следующая ветка основана на предыдущей,
  PR открывается в ветку предыдущего этапа (стек), чтобы дифф содержал только этап.
- Claude Code пишет код и офлайн-тесты; приёмку, testnet и live запускает оператор.
- Перед PR: `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `cargo deny check`, `cargo audit`.

## Ветки и PR

| Этап | Ветка | База | PR | Статус |
| --- | --- | --- | --- | --- |
| M0 | `claude/lucid-mccarthy-gx3vbm` | `main` | #1 | готов, ждёт слияния |
| M1 | `claude/m1-scanner` | M0 | см. ниже | код готов, PR открыт |
| M2 | `claude/m2-execution` | M1 | — | в работе |
| M3 | `claude/m3-live-hl-lighter` | M2 | — | не начат |
| M4 | `claude/m4-aster-ramp` | M3 | — | не начат |

## План по этапам

### M1 — сканер, теневой портфель, отчёты

- `sh-md::state` — состояние рынка из `MdEvent` (стаканы, цены, ОИ, история фандинга, метаданные).
- `sh-signals` — APR фандинга, оценки (прогноз, EMA за 24 ч, реализованное среднее за 7 д,
  устойчивость знака), модель издержек (комиссии, проскальзывание по стакану, спред на выходе,
  доля перебалансировки), `s_net`, ROC, точка безубыточности, размер позиции, фильтры пар,
  сопоставление символов (`config/symbols.toml` + автосопоставление с одобрением оператора),
  решения входа/выхода S1/S2 (общие для теневого портфеля и стратегий M2).
- `sh-portfolio` — теневой портфель на $100k по правилам S1/S2 и лимитам риска, фандинг по
  фактическим расчётам, издержки по записанным стаканам; ежедневный отчёт Markdown + CSV.
- `sh-store` — SQLite (sqlx): теневой портфель, первое появление инструментов; Parquet-производные
  из сырых записей (1 с BBO, фандинг, снимки стаканов, ОИ); повтор сырых записей.
- Повтор: парсеры повторов в каждом адаптере (`RawRecord` → `MdEvent`), слияние площадок по времени.
- `sh-obs` — Telegram (сводка), метрики сканера.
- Секреты: файл TOML, расшифровка через systemd `LoadCredentialEncrypted` или внешнюю команду
  (`age -d`), `SecretString`, `[redacted]` в Debug.
- Бинарник: `spread-hunter run` в режиме `scan`, `spread-hunter scan --replay`, `spread-hunter derive`.

### M2 — исполнение, риск, S5, paper, selftest

- `sh-venue-api::exec` — трейт `Execution`, ордера, заполнения, позиции, балансы, client ID.
- Приватные адаптеры: Hyperliquid (подпись L1-действий агент-кошельком, nonce), Lighter (FFI к
  `lighter-go`, изолированный `unsafe`), Aster (EIP-712 API-кошелька, nonce в мкс), приватные потоки.
- `sh-sim` — симулятор площадки, модели исполнения (тейкер по стакану + задержка, мейкер по
  очереди), драйвер повтора, детерминизм; бинарник `backtest`.
- `sh-strategy` (S1, S2), `sh-exec` (S5, нарезка, хедж, тайм-ауты, разворот, автомат ордера),
  `sh-risk` (проверки, мониторы, kill switch), `sh-portfolio` (позиции, сверка, PnL).
- Режим `paper`, `spread-ctl` (Unix-сокет), `spread-hunter selftest`.

### M3 — live на Hyperliquid + Lighter

- Режим `live` с защитой `--live --i-understand-real-money`, `live --dry-run`, лимиты капитала
  $5k и 2 пары, старт с обязательной сверкой, Telegram-оповещения и команды, `config/live.toml`.

### M4 — Aster live, наращивание капитала, планировщик перебалансировки

- Aster в live, ступени капитала $25k → $50k → $100k, `spread-ctl rebalance plan` с неподписанными
  данными для оператора, `spread-ctl export --year`.

## Текущее состояние

### M1 — готово (код и офлайн-тесты)

- `sh-md::state` — состояние рынка; `sh-signals` — оценки, издержки, размер, оценка пар,
  символы, сессии, `testkit` для тестов; `sh-portfolio` — теневой портфель и отчёты;
  `sh-store` — SQLite (`db`) и Parquet (`parquet_out`); `sh-core::secrets`;
  `sh-obs::alerts` (Telegram); парсеры повтора `HlReplay`, `LtReplay`, `AsReplay`.
- Корневой крейт: `scan` (движок), `runner` (режимы `record`/`scan`, повтор),
  `replay_source`, `derive`, `monitor`; CLI `run`, `scan --replay`, `derive`.
- Конфиг: новые секции `[scanner]`, `[strategy.*]`, `[costs]`, `[fees]`, `[risk]`,
  `[symbols]`, `[database]`, `[secrets]`, `[telegram]`; `engine.mode` по умолчанию `scan`
  (в M2 станет `paper`, когда режим появится).
- Документация: `docs/scanner.md`; systemd: `spread-hunter.service`, таймер Parquet.
- Не делалось (по правилам): приёмочный 30-дневный прогон — его запускает оператор.

### M2 — следующий шаг

Начать с ветки `claude/m2-execution` от `claude/m1-scanner`. Порядок: трейт `Execution`
и типы ордеров → `sh-sim` (симулятор площадки) → `sh-exec` (автомат ордера, S5) →
`sh-risk` → `sh-strategy` (S1/S2 поверх `sh-signals::evaluate`) → портфель позиций и
сверка → режим `paper` → приватные адаптеры (HL, Lighter FFI, Aster) → `selftest` →
`spread-ctl` → `backtest`.
