# market-data

Example contract for [proposal 0004](../../docs/proposals/0004-market-data.md), phase A: stock market data for any exchange, in one schema, from one provider.

- **Provider writes facts:** `publish_exchanges`, `publish_companies`, `publish_quotes`, `publish_bars`, `publish_fundamentals` (at most 500 rows per call, provider only, a whole batch is refused if one row is bad, an older quote never replaces a newer one).
- **Everyone reads, free:** `stock(symbol, include, bars)` returns company, quote, daily bars (newest first), fundamentals, ratios (P/E, P/B, dividend yield, payout, net margin), analytics (change, market cap, free float, 20-day high and drawdown, SMA 20/50, average volume, distance from the 52-week high) and the sector view (median P/E on the same exchange, premium) in **one call**. `include` is a bit mask (`0` = everything). Also `quotes(symbols)`, `symbols(exchange, limit, cursor)`, `sector(exchange, sector)`.
- **Derived numbers are computed in the query** from the published rows, with integer math only, so anyone can recompute them.

Units: prices in 1e-4 of the local currency (`312.55` = `3125500`), money totals in whole units, percentages in basis points, ratios in 1e-4. Symbols are `EXCHANGE:TICKER` (`PSX:OGDC`).

Build: `./build.sh` (writes `market-data.elf`). Tests: `cargo test -p btb-tests --test market_data` loads real PSX figures for OGDC and checks P/E 5.12, yield 4.00 %, market cap PKR 1,344,255,171,420 and the -17.8 % discount to the sector median against the off-chain API.
