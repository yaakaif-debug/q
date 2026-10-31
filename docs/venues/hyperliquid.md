# Hyperliquid (ядро + HIP-3 дексы) — справка по рыночным данным для M0

Область: только публичные рыночные данные (без подписей, без `/exchange`, без
пользовательских эндпоинтов). Все сведения проверены **2026-10-29** по официальной
документации и, где указано, по живым ответам `https://api.hyperliquid.xyz`,
полученным в тот же день. Всё, что не подтверждено ни документацией, ни живым
ответом, помечено **UNVERIFIED**.

## Эндпоинты

| Что | Значение | Источник |
| --- | --- | --- |
| REST info | `POST https://api.hyperliquid.xyz/info`, тело JSON `{"type": ...}` | [Info endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint) |
| WebSocket | `wss://api.hyperliquid.xyz/ws` (тестнет `wss://api.hyperliquid-testnet.xyz/ws`) | [Websocket](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket) |

### Запросы REST `info`, которые использует M0

| `type` | Параметры | Назначение | Вес |
| --- | --- | --- | --- |
| `perpDexs` | — | Обнаружение дексов. Элемент 0 — `null` (ядро, dex `""`), далее по объекту на каждый HIP-3 декс: `name`, `fullName`, `deployer`, `oracleUpdater`, `feeRecipient`, `assetToStreamingOiCap`, `assetToFundingMultiplier`; в живом ответе также `subDeployers`, `assetToFundingInterestRate`, `assetToFundingClamp`. | 20 |
| `meta` / `metaAndAssetCtxs` | `dex` (`""` — ядро) | Инструменты декса. Живые поля актива: `name`, `szDecimals`, `maxLeverage`, `marginTableId`, `onlyIsolated`, `marginMode` (`strictIsolated` / `noCross`), `isDelisted`, у HIP-3 ещё `growthMode`, `deployerFeeScale`, `lastFeeScaleChangeTime`, `lastGrowthModeChangeTime`. Верхний уровень: `universe`, `marginTables`, `collateralToken` (индекс спот-токена). Контекст актива: `funding`, `openInterest`, `oraclePx`, `markPx`, `midPx`, `premium`, `impactPxs`, `dayNtlVlm`, `prevDayPx`, `dayBaseVlm`. | 20 |
| `spotMeta` | — | Имя токена обеспечения по индексу `collateralToken` (USDC, USDH, USDE ...). | 20 |
| `fundingHistory` | `coin`, `startTime`, `endTime?` | Расчётный фандинг: `[{coin, fundingRate, premium, time}]`. Монеты HIP-3 — с именем `dex:COIN`. | 20 + 1 на каждые 20 элементов |
| `predictedFundings` | — | Прогноз фандинга и `nextFundingTime` по площадкам. **Только ядро** (документация: «only supported for the first perp dex»). Живые ответы (2026-10-29) содержат также `fundingIntervalHours` по площадке (`HlPerp: 1`), которого нет в примере документации. | 20 |
| `l2Book` | `coin`, `nSigFigs?`, `mantissa?` | Снимок стакана (запасной REST-путь). | 2 |

Живая проверка 2026-10-29: `perpDexs` вернул ядро и 10 HIP-3 дексов
(`xyz, flx, vntl, hyna, km, abcd, cash, para, mkts, io`). У `flx`, `vntl`, `hyna`,
`km`, `cash` и `abcd` в тот день все активы были `isDelisted`; активных активов:
ядро 179, `xyz` 119, `para` 33, `io` 12, `mkts` 5. Токены обеспечения: ядро, `xyz`,
`para`, `mkts`, `io`, `abcd` → USDC (0); `flx`, `vntl`, `km` → USDH (360);
`hyna` → USDE (235); `cash` → USDT0 (268). Адаптер этот список не использует: он
вызывает `perpDexs` при старте и каждый час.

## Каналы WebSocket, которые использует M0

Подписка: `{"method":"subscribe","subscription":{...}}`; подтверждение:
`{"channel":"subscriptionResponse","data":{...}}`; данные:
`{"channel":"<type>","data":...}`. Источник: [Subscriptions](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions).

| Подписка | Данные | Примечания |
| --- | --- | --- |
| `{"type":"l2Book","coin":C}` | `WsBook {coin, levels:[bids, asks], time}`; `WsLevel {px, sz, n}` | **Поток снимков**: каждое сообщение — полный верх стакана, 20 уровней (5 при `fast: true`), отправляется по блокам не чаще раза в 0,5 с. Номеров последовательности нет. |
| `{"type":"trades","coin":C}` | `WsTrade[] {coin, side, px, sz, hash, time, tid, users}` | `side`: `"B"` (агрессор покупал) / `"A"` (агрессор продавал) — смысл как стороны агрессора **UNVERIFIED**; в документации только `side: string`. Уникальный id — `(time, coin, tid)`. |
| `{"type":"bbo","coin":C}` | `WsBbo {coin, time, bbo:[bid|null, ask|null]}` | Отправляется, только когда BBO меняется в блоке. В M0 по умолчанию выключено (бюджет подписок, см. ниже). |
| `{"type":"allDexsAssetCtxs"}` | `{"ctxs": [[dex, [ctx, ...]], ...]}` | Одна подписка даёт прогнозный фандинг, mark/oracle и ОИ для ядра и всех HIP-3 дексов. Документация называет только тип (`WsAllDexsAssetCtxs`); форма взята из живых записей 2026-10-29: контексты идут в порядке `meta.universe` декса, поэтому адаптер сопоставляет индекс с монетой по последним метаданным. Наблюдаемая частота — примерно одно сообщение раз в 10–15 с (~140 КБ). |
| `{"type":"activeAssetCtx","coin":C}` | `{coin, ctx: PerpsAssetCtx}` | Не используется (`allDexsAssetCtxs` дешевле). |

Имена монет: ядро — `BTC`; HIP-3 — `dex:COIN`, например `xyz:NVDA`
([Asset IDs](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/asset-ids)).
Asset id для HIP-3 = `100000 + perp_dex_index * 10000 + index_in_meta`.

Пульс: сервер закрывает соединение, если 60 с не получал от клиента сообщений;
клиент шлёт `{"method":"ping"}`, сервер отвечает `{"channel":"pong"}`
([Timeouts and heartbeats](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/timeouts-and-heartbeats)).
После переподключения подтверждение подписки приносит свежий снимок.

## Лимиты (на IP)

Источник: [Rate limits and user limits](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/rate-limits-and-user-limits).

- REST: суммарный вес 1200 в минуту. `l2Book`, `allMids`, `exchangeStatus` … — вес 2; большинство прочих `info` — 20; `fundingHistory` и др. — +1 за каждые 20 возвращённых элементов.
- WebSocket: не более 10 соединений, 30 новых соединений в минуту, **1000 подписок**, 2000 клиентских сообщений в минуту, 100 одновременных post.
- Все площадки HL (ядро и каждый HIP-3 декс) делят эти лимиты: они действуют на IP.

Следствие для M0: при ~180 активных монетах ядра и ~280 монетах HIP-3 (живые
данные 2026-10-29) `l2Book` + `trades` на каждую монету — около 920 подписок,
близко к лимиту 1000. Адаптер распределяет подписки по приоритету
(`allDexsAssetCtxs` → `l2Book` → `trades` → `bbo`) в пределах
`max_subscriptions` (по умолчанию 950) и сообщает о непокрытых монетах в логах и
метрике `md_uncovered_streams`.

## Тик, лот, комиссии, интервал фандинга — откуда берутся

- **Лот**: `szDecimals` актива → шаг `10^-szDecimals` ([Tick and lot size](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/tick-and-lot-size)).
- **Правило цены**: фиксированного тика нет. До 5 значащих цифр и не более `MAX_DECIMALS - szDecimals` знаков после запятой, `MAX_DECIMALS = 6` для perp; целые цены допустимы всегда. Хранится как `PriceRule::SigFigs`.
- **Комиссии**: уровень пользователя (объём за 14 дней) из `userFees`, которому нужен адрес аккаунта → в публичных метаданных недоступно; M0 записывает только публичные входные данные. По активу M0 записывает `deployerFeeScale` и `growthMode` (HIP-3; есть в живом `meta`, нет в примере документации). Формула в [Fees](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees): множитель HIP-3 = `feeScale + 1` при `feeScale < 1`, иначе `2 * feeScale`; growth mode — ×0,1.
- **Интервал фандинга**: в `meta` его нет. Ядро: `predictedFundings[*].HlPerp.fundingIntervalHours` (живое поле, см. выше). HIP-3: **выводится** по дексу из соседних отметок `fundingHistory` (самый частый шаг по монетам декса; первая монета каждого декса опрашивается первой). Документация ([Funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding)) говорит, что фандинг платится каждый час и что «the funding cap and funding interval do not depend on the asset». 1 ч нигде не захардкожен. Живая проверка 2026-10-29: вывод даёт 1 ч для всех дексов с активными активами.
- **Прогнозный фандинг**: `funding` в контексте актива (`metaAndAssetCtxs` / `allDexsAssetCtxs`) — ставка текущего часа (за интервал, не годовая). `predictedFundings` даёт `nextFundingTime` только для ядра. Для HIP-3 следующий расчёт оценивается как последний расчёт + выведенный интервал (**UNVERIFIED**, что HIP-3 рассчитывается по той же часовой сетке, что и ядро).
- **Расчётный фандинг**: `fundingHistory` (REST). M0 опрашивает каждую монету раз в час через лимитер по весу.
- **Обеспечение**: `collateralToken` (индекс спот-токена) → `spotMeta.tokens[i].name`. Живая проверка 2026-10-29: см. список токенов выше.
- **Режим маржи**: `marginMode` / `onlyIsolated` по активу.
- **Делистинг и расчёт по mark**: `isDelisted` по активу; деплоер действием `haltTrading` закрывает позиции по mark ([HIP-3](https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals)). Как в `meta` выглядит остановленный, но не делистингованный актив, — **UNVERIFIED**.

## Открытые вопросы (UNVERIFIED)

- У `allDexsAssetCtxs` нет формальной схемы в документации (разбор по живым записям).
- Учитываются ли подписки `bbo` и `l2Book` на монеты HIP-3 в тот же лимит 1000, что и ядро (считаем, что да: лимит сформулирован на IP).
- Смысл букв `side` в сделках.
