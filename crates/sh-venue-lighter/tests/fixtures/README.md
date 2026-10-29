# Fixtures

Captured from live `mainnet.zklighter.elliot.ai` (REST `/api/v1/...`, WS
`/stream?readonly=true`) on 2026-10-29 and trimmed (fewer levels, markets,
trades) to keep them small. Message bodies are otherwise as received.

`ws_order_book_snapshot.json` (`subscribed/order_book`, trimmed to 10 levels per
side) and `ws_order_book_update_{1,2,3}.json` are consecutive: each update's
`begin_nonce` equals the previous message's `nonce`.
