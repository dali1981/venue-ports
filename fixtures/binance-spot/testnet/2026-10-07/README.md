# Binance spot testnet, 7 October 2026

Seventeen replies of `https://testnet.binance.vision`, each as `<step>-<name>.json` (the body) and `<step>-<name>.status` (the HTTP
status and a newline): 34 files.

- **What was done.** A consumer's script, with its own testnet key (never written down), read the exchange rules and the price of
  `AEROUSDT`, the account and its commission rate, tested a market buy and a market sell (`/api/v3/order/test` with
  `computeCommissionRates=true`), placed them (10 AERO bought and sold back, `newOrderRespType=FULL`), read them back (`/api/v3/order`,
  `/api/v3/myTrades`), and provoked five refusals: three `-1013 LOT_SIZE`, one `-1013 NOTIONAL` and one `-2010` insufficient balance.
  The order ids are of the form `r2rec-<timestamp>-buy` and `-sell`.
- **The one edit.** The account's `uid` is `REDACTED-UID`. Every other byte is as the venue sent it.
- **What the account is.** The testnet faucet's: 517 assets with test balances (10,000 USDT, 1 BTC, ...), commission rates of zero. Nothing in
  it is a real account. A production recording is a different thing and does not belong in this repository (`SPEC.md` §7).
- **What reads it.** `src/cex/binance/recorded.rs`; the rows are in `docs/responses/binance-spot.md`, origin `testnet`.
