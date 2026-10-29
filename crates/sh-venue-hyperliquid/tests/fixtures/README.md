# Fixtures

Captured from live `api.hyperliquid.xyz` (REST `POST /info`, WS `/ws`) on
2026-10-29 and trimmed (fewer levels, assets, dexes) to keep them small. Message
bodies are otherwise byte-for-byte as received. `ws_all_dexs_asset_ctxs.json`
keeps only dexes `""` and `xyz` with their first three contexts, matching the
first three assets of `rest_meta_and_asset_ctxs_*.json`.
