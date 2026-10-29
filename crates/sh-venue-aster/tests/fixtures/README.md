# Fixtures

Captured from live `fapi.asterdex.com` (REST) and `fstream.asterdex.com`
(combined streams) on 2026-10-29 and trimmed (fewer symbols) to keep them
small. Message bodies are otherwise as received.

`ws_depth_update_{1,2,3}.json` are consecutive (`pu` equals the previous `u`).
`rest_depth_btc.json` is an independent snapshot whose `lastUpdateId` does not
line up with those updates.
