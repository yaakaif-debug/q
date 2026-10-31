# Lighter — справка по рыночным данным для M0

Область: только публичные рыночные данные (без подписанта, без `sendTx`, без
токенов авторизации). Проверено **2026-10-29** по официальной документации
`docs.lighter.xyz` (указанный в SPEC.md адрес `apidocs.lighter.xyz/docs/environments`
теперь перенаправляет туда) и по живым ответам mainnet в тот же день.
Неподтверждённое помечено **UNVERIFIED**.

## Эндпоинты

| Что | Значение | Источник |
| --- | --- | --- |
| REST | `https://mainnet.zklighter.elliot.ai/api/v1/` | [Environments](https://docs.lighter.xyz/developers/getting-started/environments) |
| WebSocket | `wss://mainnet.zklighter.elliot.ai/stream` (вариант только для чтения для ограниченных регионов: `?readonly=true`) | там же |
| Тестнет | REST `https://testnet.zklighter.elliot.ai`, WS `.../stream`, chain id 300 | там же |
| Chain id mainnet | 304 (только для подписи; в M0 не используется) | там же |

Подсказка по размещению из документации: AWS Tokyo `ap-northeast-1`, AZ `apne1-az4`.
Трафик идёт через AWS CloudFront.

Проверено 2026-10-29 с выходом в сеть из США: `wss://.../stream` (без `readonly`)
отклоняет рукопожатие с HTTP 400 и `{"code":20558,"message":"You are accessing
Lighter from a restricted jurisdiction..."}`; `?readonly=true` работает. REST-эндпоинты
рыночных данных отвечают нормально. Адаптер по умолчанию использует `?readonly=true`.

### REST, который использует M0

| Путь | Назначение | Примечания |
| --- | --- | --- |
| `GET /api/v1/orderBookDetails?filter=perp` | Метаданные рынков | `order_book_details[]`: `symbol`, `market_id`, `status` (`active`/`inactive`), `taker_fee`, `maker_fee` (строки в процентах, базовая ставка рынка), `min_base_amount`, `min_quote_amount`, `size_decimals`, `price_decimals`, `supported_*_decimals`, `default/min_initial_margin_fraction`, `maintenance_margin_fraction`, `funding_clamp_small`, `funding_clamp_big`, `base_interest_rate`, `funding_premium_multiplier`, `mark_price`, `index_price`, `open_interest` (одна сторона, в базовом активе), `open_interest_usd`, `market_config.{market_margin_mode, force_reduce_only, trading_hours, hidden}`, `created_at` (мс, строкой). [orderBookDetails](https://docs.lighter.xyz/api/orders/orderbookdetails) |
| `GET /api/v1/fundings?market_id&resolution=1h&start_timestamp&end_timestamp&count_back` | История расчётного фандинга | `fundings[] {timestamp (с), value, rate, direction}`. [fundings](https://docs.lighter.xyz/api/funding/fundings) |
| `GET /` | Статус `{status, network_id, timestamp}` | [status](https://docs.lighter.xyz/api/system/status). `/api/v1/status` отвечает 403 от CloudFront. |

Живая проверка 2026-10-29: 241 perp-рынок, 213 со статусом `active`.

## Каналы WebSocket, которые использует M0

Конверт: `type: "connected"`, `type: "subscribed/<channel>"` (снимок),
`type: "update/<channel>"`, ошибки — `{error:{code,message}}`. Подписка:
`{"type":"subscribe","channel":"order_book/0"}`; в ответе `channel` через двоеточие:
`order_book:0`. Источник: [WebSocket](https://docs.lighter.xyz/api/websocket).

| Канал | Данные | Последовательность |
| --- | --- | --- |
| `order_book/{market_id}` | `order_book {asks[], bids[] из {price,size}, offset, nonce, begin_nonce, last_updated_at}`, `timestamp` конверта (мс) | Снимок при подписке, затем дельты каждые 50 мс; `size` — новый итог уровня, `"0"` удаляет уровень. **Непрерывность: `begin_nonce` обновления равен `nonce` предыдущего сообщения** (проверено вживую). `offset` относится к серверу API и может прыгнуть при переподключении — его не используем. При разрыве: отписка и повторная подписка ради нового снимка ([Market data guide](https://docs.lighter.xyz/developers/integration-guides/market-data)). |
| `trade/{market_id}` | `{trades[], liquidation_trades[], nonce}`; `Trade {trade_id, type, market_id, size, price, usd_amount, is_maker_ask, timestamp (мс), transaction_time (мкс), ...}` | Проверка последовательности не нужна (сделки — не состояние). Сторона агрессора: `is_maker_ask == true` → мейкер стоял на ask → агрессор покупал. **UNVERIFIED** (вывод из имени поля). |
| `market_stats/all` | `market_stats {symbol, market_id, index_price, mark_price, mid_price, open_interest, current_funding_rate, funding_rate, funding_timestamp, ...}` | `current_funding_rate` — оценка ближайшего платежа; `funding_rate` — последний расчёт в момент `funding_timestamp`. |
| `ticker/{market_id}` | BBO `{s, a{price,size}, b{price,size}}` + `nonce` | На каждый `nonce`. В M0 по умолчанию выключен (BBO берётся из `order_book`; экономит клиентские сообщения). |

`market_stats/all` присылает словарь по `market_id` (проверено вживую 2026-10-29;
документация показывает только форму для одного рынка; адаптер понимает обе).
Особенности живых данных, которые учитывает разбор: `mid_price` бывает `""` при
одностороннем стакане (каждый элемент словаря разбирается отдельно, поэтому один
плохой рынок не теряет остальные); `open_interest` здесь — одна сторона **в USD**
(`open_interest_usd = 2 * open_interest`), в отличие от
`orderBookDetails.open_interest`, который в базовом активе.

Метки времени ([Timestamps](https://docs.lighter.xyz/developers/reference/timestamps)):
`timestamp` конверта — мс (сервер API), `Trade.timestamp` — мс (блок), `transaction_time`
и `last_updated_at` стакана — мкс. `fundings[].timestamp` — **секунды** (живая проверка).

Поддержание соединения: клиент обязан присылать кадр хотя бы раз в 2 минуты;
`{"type":"ping"}` → `{"type":"pong"}`. Поддерживается `permessage-deflate`. Медленных
клиентов отключают после тайм-аутов записи 250 мс.

## Лимиты

Источник: [Rate limits](https://docs.lighter.xyz/developers/reference/rate-limits).

- REST, Standard / без авторизации: **60 запросов в скользящую минуту** на IP. Открытие `/stream` тоже считается (вес 90 для весовых уровней).
- WebSocket на IP: 255 соединений, 500 подписок на соединение, 5000 на IP, **200 клиентских сообщений в минуту**, 50 сообщений «в полёте».
- При превышении: HTTP 429 (или 405); разрыв WS. Остывание после файрвола — 60 с.

Следствие для M0: подписка `order_book` + `trade` на ~213 рынков — около 430
клиентских сообщений, то есть не меньше ~2,2 минуты подписки при лимите 200/мин.
Адаптер отправляет все клиентские сообщения Lighter (подписки, пересинхронизация,
пинги) через один общий лимитер (по умолчанию 150 в минуту, всплеск 10). Всплеск
из ~150 подписок 2026-10-29 вызвал
`{"error":{"code":30010,"message":"Too Many Inflight Messages!"}}` и сброс
соединения — отсюда маленький всплеск.

## Тик, лот, комиссии, интервал фандинга — откуда берутся

- **Тик / лот**: целочисленное кодирование с `price_decimals` / `size_decimals` → тик `10^-price_decimals`, лот `10^-size_decimals`; минимумы — `min_base_amount`, `min_quote_amount` ([Market data](https://docs.lighter.xyz/developers/integration-guides/market-data)).
- **Комиссии**: `maker_fee` / `taker_fee` рынка в `orderBookDetails` (в процентах). Поверх действует тип аккаунта. Документация (2026-10-29): Standard платит 0 / 0 на обычных ордерах, **задержка тейкера 300 мс, мейкера 0 мс, отмены/изменения 300 мс**; Premium — 0,0040% / 0,0280% при 0 LIT, задержка тейкера 140 мс ([Trading fees](https://docs.lighter.xyz/trading/trading-fees)). SPEC.md приведён в соответствие с этими данными.
- **Интервал фандинга**: в метаданных его нет. Документация: «Funding payments occur at each hour mark» ([Funding](https://docs.lighter.xyz/trading/funding)). Адаптер выводит интервал по рынку из соседних отметок расчётов — при старте через `fundings` (с учётом лимита) и далее по изменениям `market_stats.funding_timestamp`. Ничего не захардкожено.
- **Единицы ставки фандинга**: `fundings[].rate` и `market_stats.*funding_rate` — **проценты за интервал**. Проверено вживую 2026-10-29: рынок 0, `rate "0.0012"`, `value "0.028254"` = 0,0012% × индекс ETH ≈ 2354. Нормализованные события содержат долю (`rate / 100`).
- **Направление фандинга**: `fundings[].direction` — `long`/`short`; называет ли оно платящую сторону — **UNVERIFIED**.
- **Ограничители и процентная ставка**: `funding_clamp_small`, `funding_clamp_big`, `base_interest_rate` (в процентах) по рынку.
- **Обеспечение**: USDC (perp). Мультиактивная маржа существует ([Multi-asset margin](https://docs.lighter.xyz/trading/multi-asset-margin)) — в M0 не используется.

## Открытые вопросы (UNVERIFIED)

- Сторона агрессора по `is_maker_ask`.
- Считаются ли управляющие кадры ping WebSocket в лимит 200 сообщений в минуту.
- Единицы `min_initial_margin_fraction`: 200 трактуется как 2% → максимальное плечо 50.
