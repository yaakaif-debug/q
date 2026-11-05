# Aster (фьючерсы v3) — справка по рыночным данным для M0

Область: только публичные рыночные данные (без API-кошелька, без EIP-712, без
подписанных эндпоинтов). Проверено **2026-10-29** по официальной документации и
живым ответам `https://fapi.asterdex.com` в тот же день. Неподтверждённое помечено
**UNVERIFIED**.

Основной источник: [Futures API v3 (EN), GitHub `asterdex/api-docs`](https://github.com/asterdex/api-docs/blob/master/V3(Recommended)/EN/aster-finance-futures-api-v3.md)
(веб-версия: [asterdex.github.io/aster-api-website/futures-v3/general-info](https://asterdex.github.io/aster-api-website/futures-v3/general-info/)).
По форме протокол совместим с Binance USDⓈ-M (заголовки `X-MBX-*`, идентификаторы
глубины `U/u/pu`).

## Эндпоинты

| Что | Значение |
| --- | --- |
| REST | `https://fapi.asterdex.com` (пути v3 — `/fapi/v3/...`) |
| WebSocket | `wss://fstream.asterdex.com` — одиночный поток `/ws/<stream>` или комбинированный `/stream?streams=a/b/c` (комбинированный оборачивает как `{"stream":..., "data":...}`) |
| Тестнет | `https://fapi.asterdex-testnet.com` (из SPEC.md; в M0 не используется) |

### REST, который использует M0

| Путь | Вес | Назначение |
| --- | --- | --- |
| `GET /fapi/v3/exchangeInfo` | 1 | Символы, фильтры, `rateLimits`. По символу: `symbol`, `status` (`TRADING` ...), `contractType` (`PERPETUAL`), `baseAsset`, `quoteAsset`, `marginAsset`, `onboardDate`, `filters` (`PRICE_FILTER.tickSize`, `LOT_SIZE.stepSize/minQty`, `MIN_NOTIONAL.notional` ...). `pricePrecision`/`quantityPrecision` **нельзя** использовать как тик/шаг (документация). |
| `GET /fapi/v3/fundingInfo` | 1 | По символу: `fundingIntervalHours`, `interestRate`, `fundingFeeCap`, `fundingFeeFloor`. **Источник интервала фандинга.** |
| `GET /fapi/v3/premiumIndex` | 1 | `markPrice`, `indexPrice`, `lastFundingRate`, `nextFundingTime`, `interestRate`, `time`. |
| `GET /fapi/v3/fundingRate?symbol&startTime&endTime&limit` | 1 | История расчётного фандинга `{symbol, fundingRate, fundingTime}`. |
| `GET /fapi/v3/depth?symbol&limit` | 2 (≤50), 5 (100), 10 (500), 20 (1000) | Снимок для синхронизации локального стакана (`lastUpdateId`, `E`, `T`, `bids`, `asks`). |
| `GET /fapi/v1/openInterest?symbol` | **UNVERIFIED** | `{symbol, openInterest, time}`. В документации v3 нет; вживую работает и на `/fapi/v1`, и на `/fapi/v3` (2026-10-29). |
| `GET /fapi/v3/time`, `/fapi/v3/ping` | 1 | Замер задержки. |

Живая проверка 2026-10-29: `exchangeInfo` содержит 644 символа, 602 со статусом
`TRADING`. Встреченные значения `marginAsset`: `USDT`, `USDC`, `USD1`, `U` —
**не все контракты маржинальны в USDT**. Обеспечение берётся из `marginAsset`
каждого символа; SPEC.md исправлен соответственно.

## Потоки WebSocket, которые использует M0

Источник: раздел «Websocket Market Streams» документации v3.

| Поток | Данные | Примечания |
| --- | --- | --- |
| `<symbol>@depth@100ms` | `depthUpdate {E, T, s, U, u, pu, b[[px,qty]], a[[px,qty]]}` | Дельты глубины. Количества абсолютные; 0 удаляет уровень. Включено по умолчанию. |
| `<symbol>@aggTrade` | `aggTrade {E, s, a, p, q, f, l, T, m}` | `m = true` → покупатель — мейкер → агрессор продавал. |
| `!markPrice@arr@1s` | `[markPriceUpdate {E, s, p (mark), i (индекс), P, r (ставка фандинга), T (время следующего расчёта)}]` | Все символы, раз в 1 с. Прогнозный фандинг, mark и индекс. |
| `!bookTicker` | `bookTicker {u, E, T, s, b, B, a, A}` | BBO всех символов в реальном времени. **По умолчанию выключен** (`subscribe_book_ticker = false`): это около половины объёма записи Aster, а BBO с шагом 100 мс уже есть в локальном стакане из `@depth@100ms`. |

Процедура локального стакана (документация, «How to manage a local order book
correctly»): буферизовать события потока → GET `/fapi/v3/depth` → отбросить события
с `u < lastUpdateId` → первое применяемое событие должно удовлетворять
`U <= lastUpdateId <= u` → далее `pu` каждого события равен `u` предыдущего,
иначе заново синхронизироваться по новому снимку. Непрерывность цепочки `pu`
проверена вживую.

Правила соединений: одно соединение живёт не дольше **24 ч**; сервер шлёт кадр ping
каждые 5 минут и разрывает соединение, если 15 минут нет pong; не более
**10 входящих клиентских сообщений в секунду** на соединение; не более **200
потоков на соединение**. Символы в именах потоков — в нижнем регистре.

M0 подписывается через URL комбинированного потока (без сообщений SUBSCRIBE), не
более `streams_per_connection` (по умолчанию 180) потоков на соединение, и
переподключает каждое соединение до отметки 24 ч (`max_connection_age_secs`, по
умолчанию 23 ч 30 мин, со сдвигом между соединениями). Максимальное число
соединений на IP — **UNVERIFIED** (не документировано).

## Лимиты

- `exchangeInfo.rateLimits` (вживую 2026-10-29): `REQUEST_WEIGHT` 2400 в минуту, `ORDERS` 1200 в минуту и 300 за 10 с. Лимиты действуют на IP. 429 → отступить; повторы → бан IP 418 (от 2 минут до 3 суток). Заголовок `X-MBX-USED-WEIGHT-1M` показывает расход.
- M0 пропускает все REST-запросы Aster через один весовой лимитер (по умолчанию 1500 в минуту, ~60% опубликованного лимита).

## Тик, лот, комиссии, интервал фандинга — откуда берутся

- **Тик**: `PRICE_FILTER.tickSize`; **лот**: `LOT_SIZE.stepSize`, `minQty`; **минимальный номинал**: `MIN_NOTIONAL.notional`.
- **Интервал фандинга**: `fundingInfo.fundingIntervalHours` по символу (вживую встречаются 4 и 8).
- **Прогнозный фандинг**: `markPriceUpdate.r` / `premiumIndex.lastFundingRate` (доля за интервал). Несмотря на имя, это текущий (прогнозный) показатель: вживую 2026-10-29 у BTCUSDT `lastFundingRate = 0.00005371` (как и `r`), а три последних расчёта в `fundingRate` были `0.0001`. Расчётные значения берутся из истории `fundingRate`.
- **Комиссии**: только через `GET /fapi/v3/commissionRate` (USER_DATA, подписанный запрос) → в M0 недоступны. 0% / 0,04% из SPEC.md не берутся из метаданных и не используются. Вопрос для M1.
- **Обеспечение**: `marginAsset` по символу (USDT, USDC, USD1, U).

## Ключи и права

В M0 не затрагиваются. По README документации (2026-10-29): с 2026-09-01 все
авторизованные эндпоинты v3 требуют, чтобы основной кошелёк сделал депозит;
публичные рыночные данные это не затрагивает.

## Открытые вопросы (UNVERIFIED)

- Вес `openInterest`.
- Максимум соединений WebSocket на IP.
- Доступен ли `@depth@100ms` для всех символов (документация: «100 ms (if existing)»).

## Приватный API (M2)

Адаптер `sh-venue-aster::exec` (код написан, на площадке не проверялся).

- **Ключи**: API-кошелёк (`signer`), авторизованный основным кошельком (`user`). Вывод через API
  требует подписи основного кошелька, поэтому подписант сам вывести средства не может.
  Срок действия и точные права подписанта — **UNVERIFIED**; оператор проверяет их на странице
  API-кошелька Aster.
- **Подпись** (Aster API v3, «Authentication signature payload»): строка параметров в порядке
  отправки (`urllib.parse.urlencode`), к ней добавляются `nonce` (микросекунды), `user`, `signer`;
  EIP-712 `Message { string msg }`, домен `AsterSignTransaction`, версия `1`, chainId 1666, нулевой
  `verifyingContract`; подпись — `0x` + 65 байт `r‖s‖v` в параметре `signature`. Включение `user`
  в строку (в примере документации его нет) и формат `signature` — **UNVERIFIED**. Тест проверяет,
  что подпись восстанавливается в адрес API-кошелька и что ключ соответствует `signer`.
- **Nonce**: микросекунды, строго возрастают; окно площадки ±60 с, последние 100 nonce на агента.
- **Хост**: `https://fapi3.asterdex.com` (как в примере документации v3, **UNVERIFIED**); testnet-хосты —
  **UNVERIFIED**, задаются в `execution.aster_rest` / `execution.aster_ws`.
- **Эндпоинты** (Binance-совместимые): `POST /fapi/v3/order` (`timeInForce` `GTX` — post-only, `IOC`,
  `GTC`; `newClientOrderId`), `DELETE /fapi/v3/order`, `GET /fapi/v3/openOrders` и
  `DELETE /fapi/v3/allOpenOrders` по символам (отмена всех), `GET /fapi/v3/order` (код −2013 —
  ордер неизвестен), `GET /fapi/v3/positionRisk`, `GET /fapi/v3/balance`, `GET /fapi/v3/income`
  (`FUNDING_FEE`), `POST /fapi/v3/listenKey`.
- **Пользовательский поток**: `wss://fstream.asterdex.com/ws/<listenKey>`, событие
  `ORDER_TRADE_UPDATE` (заполнение при `x = TRADE`). Новый listenKey при каждом подключении,
  продление не делается (**UNVERIFIED**: срок жизни ключа). `ACCOUNT_UPDATE` не используется —
  позиции берутся REST-снимком для сверки.
- **Обеспечение** — по активу баланса (USDT, USDC, USD1, U); капитал = `balance + crossUnPnl`.
