# Binance spot: what each call answers, and where this crate has seen it

One row for each **call × response variant**. This is the catalogue `specs/V7-production-validation.md`
describes; `catalogue_matches_fixtures` (`src/catalogue.rs`) keeps it honest: every file under `fixtures/`
has a row, every row's body exists, and every row names a test that exists and reads that body.

**Origin** says how the body came to be:

- `production` — recorded from `https://api.binance.com` by the production tier (`VP_RECORD_DIR`).
- `testnet` — recorded from `https://testnet.binance.vision`.
- `documented` — copied from Binance's documentation; nothing in the crate has seen the venue say it.
- `synthetic` — written for a test, to reach a case the venue's documentation does not show.
- `not provokable` — a case this tier does not provoke, on purpose; there is no body.

A row whose origin is `documented`, `synthetic` or `not provokable` stays in the table: the gap is
visible. The consumer adds its own columns in its own repository. This table is **not** called
`provenance`, which already means `Simulated` or `Landed`.

**State of the recordings.** No testnet or production body is in this repository yet. The consumer's
recording of the spot testnet (7 October 2026, 34 files, `REDACTED-UID`) has not been supplied
(`specs/V7-questions.md`, Q1); the six test bodies that were marked `TODO(R2)` are the `documented`
rows below, and each is replaced by its recorded body when the recording is copied to
`fixtures/binance-spot/testnet/2026-10-07/`.

| Call | Variant | Body | Origin | Test | Typed result |
| --- | --- | --- | --- | --- | --- |
| `GET /api/v3/order` | a LIMIT order still working (`NEW`) | `fixtures/binance-spot/documented/query-order-working.jsonc` | documented | `cex::binance::live::tests::an_order_the_venue_reports_working_is_open` | `OrderState::Open` |
| `POST /api/v3/order` (`newOrderRespType=FULL`) | a MARKET order filled in five trades | `fixtures/binance-spot/documented/new-order-full-market-filled.jsonc` | documented | `cex::binance::live::tests::a_filled_order_is_filled_with_its_fill_read_as_execute_reads_it` | `OrderState::Filled(CexFill)` |
| `GET /api/v3/myTrades` | the trade lines of one order | `fixtures/binance-spot/documented/account-trade-list.json` | documented | `cex::binance::live::tests::a_filled_order_with_no_fills_reads_its_trade_lines_from_my_trades` | `OrderState::Filled(CexFill)` |
| `GET /api/v3/order` | `-2013` no such order | `fixtures/binance-spot/documented/error-minus-2013-no-such-order.json` | documented | `cex::binance::live::tests::not_found_is_conclusive_only_after_the_receive_window_and_the_clock_bound` | `OrderState::NotFound` once `recvWindow` and the clock bound have passed, else `OrderState::Open` |
| `GET /api/v3/account` | the account, with balances | `fixtures/binance-spot/documented/account-information.json` | documented | `cex::binance::balances::tests::the_documented_account_reads_as_exact_free_and_locked_balances` | `SpotAccountBalances` |
| `GET /api/v3/account` | `-1121` refused | `fixtures/binance-spot/documented/error-minus-1121-invalid-symbol.json` | documented | `cex::binance::balances::tests::a_refused_read_is_an_error_carrying_the_venues_code` | an `Err` carrying `-1121` |
| any | `418` after repeated `429`s (a ban) | — | not provokable | — | — |
| any | a `429` storm | — | not provokable | — | — |
| `POST /api/v3/order` | a repeated client order id after a fill (Binance accepts it, so proving it would trade twice) | — | not provokable | — | — |
| `POST /api/v3/order` | a partial fill, or a market order beyond the book | — | not provokable | — | — |
