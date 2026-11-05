# Live на Hyperliquid + Lighter (M3)

Код M3 написан и проверен только офлайн (оболочка live против симулятора, подключённого
как приватные адаптеры, — `tests/live_shell.rs`). Ни одного реального ордера Claude Code
не отправлял; live, dry-run и selftest запускает оператор (SPEC.md, «Testing responsibility»).

## Защита

Live включается только при выполнении **всех** условий:

1. `engine.mode = "live"` (наложение `config/live.toml`);
2. флаги `spread-hunter run --live --i-understand-real-money`;
3. на площадке `venues.<площадка>.trading_enabled = true` (по умолчанию `false` у всех).

Любое несовпадение режима и флагов — отказ запуска с понятной ошибкой. Aster в live — только
с M4 (проверка конфига отклоняет `venues.aster.trading_enabled = true`, движок держит Aster
выключенным в live).

Ступень M3 (`[stage]`): капитал в парах не больше $5000 и не больше 2 пар. Её проверяет риск
перед каждым входом; размер пары тоже ограничен остатком ступени.

## Старт и остановка

1. Конфиг и секреты; приватные адаптеры только для площадок с `trading_enabled = true`.
2. Рыночные данные и метаданные (реестр метаданных для адаптеров наполняется из потока).
3. Сверка позиций со всеми торгуемыми площадками: пока она не пройдёт, стратегии не входят;
   расхождение двух проверок подряд — `Halted` и CRIT.
4. Риск по сверенной книге, затем стратегии.

SIGTERM: новые входы останавливаются, post-only ордера снимаются (до 5 с ожидания ответов),
позиции остаются, состояние сохраняется (`engine_state_live` в `data/state/live.sqlite`).
После перезапуска живые ордера переводятся в `Unknown` и уточняются запросом по клиентскому ID.

## Исполнение

Каждая команда движка — отдельная задача tokio с тайм-аутом 10 с (тайм-аут → `Unknown` →
запрос). Приватные потоки площадок дают ордера, заполнения и фандинг; снимки позиций и
балансов — каждые `risk.reconcile_every_s`. Метрика `order_ack_ms` — время ответа на ордер.

## live --dry-run

```bash
./target/release/spread-hunter --config config/default.toml --overlay config/live.toml run --live --dry-run
```

Настоящие ключи и метаданные; исполнение — симулятор (как paper, отдельное состояние
`engine_state_dry_run`), а каждая команда (ордер, отмена) строится и подписывается
приватным адаптером с `dry_run = true` и только журналируется (`dry run: signed … not sent`).
Так проверяются подпись и сериализация без отправки.

## Шаги оператора (Ubuntu 24.04)

Предполагается установка из README (пользователь `spreadhunter`, `/opt/spread-hunter`,
зашифрованные секреты).

1. Пройти paper (M2) 14 дней и учения из docs/execution.md.
2. Секреты: `[hyperliquid]` (агент-кошелёк, одобренный основным аккаунтом; основной ключ —
   на аппаратном кошельке) и `[lighter]` (отдельный индекс API-ключа), плюс `[telegram]`.
   Перешифровать файл секретов (`systemd-creds encrypt`, см. README).
3. selftest на testnet, затем на mainnet малым номиналом:

```bash
sudo -u spreadhunter /opt/spread-hunter/bin/spread-hunter --config /opt/spread-hunter/config/default.toml selftest --network testnet --max-notional-usd 20 --venue hyperliquid --transfer-dex xyz
```

   (для mainnet — `--network mainnet --i-understand-real-money`; источник секретов в конфиге
   должен быть доступен этому пользователю, например `command:age -d …`).
4. Скопировать наложение и unit:

```bash
sudo cp config/live.toml /opt/spread-hunter/config/
```

```bash
sudo cp ops/systemd/spread-hunter-live.service /etc/systemd/system/
```

5. В `/opt/spread-hunter/config/live.toml` включить `trading_enabled = true` для площадок,
   прошедших selftest.
6. Dry-run на час (из-под пользователя службы), проверить журнал подписей и `spread-ctl status`.
7. Остановить paper и запустить live (unit конфликтует с paper):

```bash
sudo systemctl daemon-reload
```

```bash
sudo systemctl disable --now spread-hunter
```

```bash
sudo systemctl enable --now spread-hunter-live
```

8. Контроль: `spread-ctl --socket /opt/spread-hunter/run/spread-hunter.sock status`, метрики
   `unhedged_usd`, `net_delta_usd`, `orders_unknown`, `order_ack_ms`, Telegram.
9. Остановка торговли: `spread-ctl kill` (или `--flatten`), файл `run/KILL`, `/kill` в Telegram.

Приёмка M3 (SPEC.md): 14 дней live; реализованное проскальзывание в пределах 30% от модели
paper; нет открытых CRIT-инцидентов.

## UNVERIFIED в M3

- Всё из «Приватный API (M2)» в docs/venues/hyperliquid.md и docs/venues/lighter.md.
- Поведение SIGTERM на площадке: отмена post-only за 5 с может не успеть при деградации —
  оставшиеся ордера уточнятся после перезапуска.
- Lighter: токен потока выдаётся на 7 ч; если площадка ограничивает срок короче, поток
  переподключится с новым токеном.
