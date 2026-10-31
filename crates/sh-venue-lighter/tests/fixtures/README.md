# Фикстуры

Записаны с живого `mainnet.zklighter.elliot.ai` (REST `/api/v1/...`, WS
`/stream?readonly=true`) 2026-10-29 и урезаны (меньше уровней, рынков, сделок),
чтобы оставаться маленькими. В остальном тела сообщений — как получены.

`ws_order_book_snapshot.json` (`subscribed/order_book`, урезан до 10 уровней на
сторону) и `ws_order_book_update_{1,2,3}.json` идут подряд: `begin_nonce` каждого
обновления равен `nonce` предыдущего сообщения.
