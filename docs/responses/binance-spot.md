# Binance spot: what each call answers, and where this crate has seen it

One row for each **call × response variant**. This is the catalogue `specs/V7-production-validation.md`
describes; `catalogue_matches_fixtures` (`src/catalogue.rs`) keeps it honest: every file under `fixtures/`
has a row, every row's body exists, and every row names a test that exists and reads that body.

**Origin** says how the body came to be:

- `production` — recorded from `https://api.binance.com` by the production tier (`VP_RECORD_DIR`). Not
  while this repository is public: a production recording holds the account's balances and rates (`SPEC.md` §7).
- `testnet` — recorded from `https://testnet.binance.vision`.
- `documented` — copied from Binance's documentation; nothing in the crate has seen the venue say it.
- `synthetic` — written for a test, to reach a case the venue's documentation does not show.
- `not provokable` — a case this tier does not provoke, on purpose; there is no body.

A row whose origin is `documented`, `synthetic` or `not provokable` stays in the table: the gap is
visible. The consumer adds its own columns in its own repository. This table is **not** called
`provenance`, which already means `Simulated` or `Landed`.

**State of the recordings.** The consumer's recording of the spot testnet (7 October 2026, 34 files, `REDACTED-UID`) is in
`fixtures/binance-spot/testnet/2026-10-07/`, with the rows of its own section below. No production body is in this repository,
and none will be while it is public (`SPEC.md` §7). The six bodies that were marked `TODO(R2)` are the `documented` rows of the
first table; the recording replaces none of them (see the section), because it holds no counterpart for three and the other
three were read by tests that assert their figures.

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

## The spot testnet recording of 7 October 2026

Seventeen replies from `https://testnet.binance.vision`, each as a `.json` body and a `.status` (`fixtures/binance-spot/testnet/2026-10-07/`,
its `README.md`). Every body is what the venue sent but for the account's `uid`; the account is the testnet faucet's. Each is read by a test in
`src/cex/binance/recorded.rs`, which serves it with the status its own `.status` file holds. None of the existing tests was changed: the
`documented` rows above keep the five-trade order, the working LIMIT order and the two refusals the recording has no counterpart for.

| Call | Variant | Body | Origin | Test | Typed result |
| --- | --- | --- | --- | --- | --- |
| `GET /api/v3/exchangeInfo?symbol=` | AEROUSDT, with its filters | `fixtures/binance-spot/testnet/2026-10-07/00-exchange-info.json` | testnet | `cex::binance::recorded::the_recorded_exchange_info_reads_as_the_rules_of_aerousdt` | `SymbolRules` |
| `GET /api/v3/ticker/price?symbol=` | the last price (no typed read in this crate) | `fixtures/binance-spot/testnet/2026-10-07/00-ticker-price.json` | testnet | `cex::binance::recorded::the_recorded_ticker_price_is_a_symbol_and_a_decimal_in_a_string` | — |
| `GET /api/v3/account` | the testnet account: 517 faucet balances, a non-ASCII asset among them | `fixtures/binance-spot/testnet/2026-10-07/01-account.json` | testnet | `cex::binance::recorded::the_recorded_account_reads_as_exact_balances_whatever_the_assets_are_called` | `SpotAccountBalances` |
| `GET /api/v3/account/commission` | rates of zero and `"discountAsset": null` | `fixtures/binance-spot/testnet/2026-10-07/02-account-commission.json` | testnet | `cex::binance::recorded::the_recorded_commission_reads_with_a_null_discount_asset` | `SymbolCommission` |
| `POST /api/v3/order/test` | `computeCommissionRates=true`, a market buy | `fixtures/binance-spot/testnet/2026-10-07/03-order-test-buy.json` | testnet | `cex::binance::recorded::the_recorded_order_tests_read_as_the_commission_rates_of_an_order` | `CommissionRates` |
| `POST /api/v3/order/test` | `computeCommissionRates=true`, a market sell | `fixtures/binance-spot/testnet/2026-10-07/03-order-test-sell.json` | testnet | `cex::binance::recorded::the_recorded_order_tests_read_as_the_commission_rates_of_an_order` | `CommissionRates` |
| `POST /api/v3/order` (`newOrderRespType=FULL`) | a market buy filled in one trade | `fixtures/binance-spot/testnet/2026-10-07/04-order-market-buy.json` | testnet | `cex::binance::recorded::the_recorded_market_orders_fill_as_execute_reads_them` | `CexFill` |
| `POST /api/v3/order` (`newOrderRespType=FULL`) | a market sell filled in one trade | `fixtures/binance-spot/testnet/2026-10-07/04-order-market-sell.json` | testnet | `cex::binance::recorded::the_recorded_market_orders_fill_as_execute_reads_them` | `CexFill` |
| `GET /api/v3/myTrades` | the trade line of the buy | `fixtures/binance-spot/testnet/2026-10-07/05-my-trades-buy.json` | testnet | `cex::binance::recorded::the_recorded_order_queries_read_as_filled_with_their_trade_lines` | `OrderState::Filled(CexFill)` |
| `GET /api/v3/myTrades` | the trade line of the sell | `fixtures/binance-spot/testnet/2026-10-07/05-my-trades-sell.json` | testnet | `cex::binance::recorded::the_recorded_order_queries_read_as_filled_with_their_trade_lines` | `OrderState::Filled(CexFill)` |
| `GET /api/v3/order` | the buy, `FILLED`, no fills listed | `fixtures/binance-spot/testnet/2026-10-07/05-order-query-buy.json` | testnet | `cex::binance::recorded::the_recorded_order_queries_read_as_filled_with_their_trade_lines` | `OrderState::Filled(CexFill)` |
| `GET /api/v3/order` | the sell, `FILLED`, no fills listed | `fixtures/binance-spot/testnet/2026-10-07/05-order-query-sell.json` | testnet | `cex::binance::recorded::the_recorded_order_queries_read_as_filled_with_their_trade_lines` | `OrderState::Filled(CexFill)` |
| `POST /api/v3/order` | `-1013` `LOT_SIZE`, a quantity under the minimum | `fixtures/binance-spot/testnet/2026-10-07/06-order-lot-size-below-min.json` | testnet | `cex::binance::recorded::the_recorded_refusals_are_venue_refusals_carrying_the_venues_code` | `VenueRefusal{code: -1013}` |
| `POST /api/v3/order` | `-1013` `LOT_SIZE`, a quantity off the step | `fixtures/binance-spot/testnet/2026-10-07/06-order-lot-size-off-step.json` | testnet | `cex::binance::recorded::the_recorded_refusals_are_venue_refusals_carrying_the_venues_code` | `VenueRefusal{code: -1013}` |
| `POST /api/v3/order` | `-1013` `LOT_SIZE`, a market order's quantity | `fixtures/binance-spot/testnet/2026-10-07/06-order-market-lot-size.json` | testnet | `cex::binance::recorded::the_recorded_refusals_are_venue_refusals_carrying_the_venues_code` | `VenueRefusal{code: -1013}` |
| `POST /api/v3/order` | `-1013` `NOTIONAL`, under the minimum notional | `fixtures/binance-spot/testnet/2026-10-07/06-order-notional.json` | testnet | `cex::binance::recorded::the_recorded_refusals_are_venue_refusals_carrying_the_venues_code` | `VenueRefusal{code: -1013}` |
| `POST /api/v3/order` | `-2010` insufficient balance | `fixtures/binance-spot/testnet/2026-10-07/07-order-insufficient-balance.json` | testnet | `cex::binance::recorded::the_recorded_refusals_are_venue_refusals_carrying_the_venues_code` | `VenueRefusal{code: -2010}` |
| `GET /api/v3/exchangeInfo?symbol=` | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/00-exchange-info.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `GET /api/v3/ticker/price?symbol=` | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/00-ticker-price.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `GET /api/v3/account` | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/01-account.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `GET /api/v3/account/commission` | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/02-account-commission.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `POST /api/v3/order/test` | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/03-order-test-buy.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `POST /api/v3/order/test` | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/03-order-test-sell.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `POST /api/v3/order` (`newOrderRespType=FULL`) | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/04-order-market-buy.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `POST /api/v3/order` (`newOrderRespType=FULL`) | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/04-order-market-sell.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `GET /api/v3/myTrades` | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/05-my-trades-buy.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `GET /api/v3/myTrades` | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/05-my-trades-sell.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `GET /api/v3/order` | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/05-order-query-buy.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `GET /api/v3/order` | the HTTP status of that reply, `200` | `fixtures/binance-spot/testnet/2026-10-07/05-order-query-sell.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `POST /api/v3/order` | the HTTP status of that reply, `400` | `fixtures/binance-spot/testnet/2026-10-07/06-order-lot-size-below-min.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `POST /api/v3/order` | the HTTP status of that reply, `400` | `fixtures/binance-spot/testnet/2026-10-07/06-order-lot-size-off-step.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `POST /api/v3/order` | the HTTP status of that reply, `400` | `fixtures/binance-spot/testnet/2026-10-07/06-order-market-lot-size.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `POST /api/v3/order` | the HTTP status of that reply, `400` | `fixtures/binance-spot/testnet/2026-10-07/06-order-notional.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |
| `POST /api/v3/order` | the HTTP status of that reply, `400` | `fixtures/binance-spot/testnet/2026-10-07/07-order-insufficient-balance.status` | testnet | `cex::binance::recorded::every_recorded_reply_carries_the_status_its_test_serves` | the status the other tests serve it with |

## The reads of V7 and the cases of the production tier

The reads added by V7 (`SPEC.md` §6d) and the replies the production tier's self-tests are served. **No body
here is a file**: each is built in the test that names it, from Binance's documentation (`documented`) or
from its field names where the documentation prints no whole answer (`synthetic`). Nothing in this table
was said by the venue to this crate.

The refusal rows below use the code and message `errors.md` documents. **The HTTP status the self-tests
serve with each (`401` for `-2015` and `-2014`, `400` for the rest) is assumed, not documented**; the first
production run records the real one (`VenueRefusal.status`), and these rows become `production`.

| Call | Variant | Body | Origin | Test | Typed result |
| --- | --- | --- | --- | --- | --- |
| `GET /api/v3/account/commission` | the documented answer for a symbol | — | documented | `cex::binance::reads::tests::the_documented_commission_reads_as_exact_rates` | `SymbolCommission` |
| `GET /api/v3/account/commission` | a refusal | — | documented | `cex::binance::reads::tests::a_refused_commission_read_is_a_venue_refusal` | an `Err` carrying a `VenueRefusal` |
| `GET /sapi/v1/account/apiRestrictions` | a key's flags (the Wallet page was not reachable: the field names are the documented ones) | — | synthetic | `cex::binance::reads::tests::the_restrictions_a_key_has_read_as_flags` | `ApiRestrictions` |
| `GET /sapi/v1/account/apiRestrictions` | a host with no `/sapi` (the testnet) | — | synthetic | `cex::binance::reads::tests::a_host_with_no_sapi_is_a_refusal` | an `Err` carrying a `VenueRefusal` |
| `GET /api/v3/ticker/bookTicker` | the top of the book | — | documented | `cex::binance::reads::tests::the_documented_book_ticker_reads_as_exact_decimals` | `BookTicker` |
| `GET /api/v3/exchangeInfo?symbol=` | one symbol, its lot and notional filters (the documentation prints no whole answer) | — | synthetic | `cex::binance::reads::tests::a_symbols_rules_read_as_step_minimums_and_status` | `SymbolRules` |
| `GET /api/v3/exchangeInfo?symbol=` | `-1121` unknown symbol | — | documented | `cex::binance::reads::tests::an_unknown_symbol_is_a_refusal_carrying_the_venues_code` | an `Err` carrying `VenueRefusal{code: -1121}` |
| `GET /api/v3/trades` | the public trade list | — | documented | `cex::binance::reads::tests::the_documented_trades_read_as_exact_decimals_and_a_limit` | `Vec<PublicTrade>` |
| `GET /api/v3/depth` | a book's levels and `lastUpdateId` | — | documented | `cex::binance::reads::tests::the_documented_book_reads_as_levels_best_first_with_its_update_id` | `OrderBookSnapshot` |
| any signed | `-1102` no signature (B1) | — | documented | `production::lv1_binance::tests::b1_a_request_with_no_signature_is_refused_1102` | `VenueRefusal{code: -1102}` |
| any signed | `-1022` wrong signature (B2) | — | documented | `production::lv1_binance::tests::b2_a_wrong_signature_is_refused_1022` | `VenueRefusal{code: -1022}` |
| any signed | `-1021` timestamp outside `recvWindow`, not retried (B3) | — | documented | `production::lv1_binance::tests::b3_a_timestamp_ten_seconds_old_is_refused_1021_and_not_retried` | `VenueRefusal{code: -1021}` |
| any signed | `-2015` a key from a host that is not whitelisted (B4) | — | documented | `production::lv1_binance::tests::b4_a_key_from_a_host_that_is_not_whitelisted_is_refused_2015` | `VenueRefusal{code: -2015}` |
| `POST /api/v3/order/test` | `-2015` a key that may not trade (B5) | — | documented | `production::lv1_binance::tests::b5_an_order_test_with_a_key_that_may_not_trade_is_refused_2015` | `VenueRefusal{code: -2015}` |
| `POST /api/v3/order` | `-1013` under the minimum notional (B6) | — | documented | `production::lv1_binance::tests::b6_an_order_under_the_minimum_notional_is_refused_1013` | `VenueRefusal{code: -1013}` |
| `POST /api/v3/order` | `-1013` a quantity off the lot step (B6) | — | documented | `production::lv1_binance::tests::b6_a_quantity_off_the_lot_step_is_refused_1013_for_the_step_alone` | `VenueRefusal{code: -1013}` |
| `POST /api/v3/order` | `-2010` a market order on an empty account (B7) | — | documented | `production::lv1_binance::tests::b7_a_market_order_on_an_empty_account_is_refused_2010` | `VenueRefusal{code: -2010}` |
| `POST /api/v3/order` | `-1121` an unknown symbol (B8) | — | documented | `production::lv1_binance::tests::b8_an_unknown_symbol_is_refused_1121` | `VenueRefusal{code: -1121}` |
| `POST /api/v3/order/test` | a valid order is accepted, and the commission rates are read (B9) | — | synthetic | `production::lv1_binance::tests::b9_a_valid_order_test_is_accepted_and_its_rates_are_read` | accepted; `SymbolCommission` |
| the six reads | a key with no withdrawals, an IP restriction and a symbol that trades (B10) | — | synthetic | `production::lv1_binance::tests::b10_the_reads_answer` | each read ok |
| `POST /api/v3/order`, `GET /api/v3/order`, `GET /api/v3/myTrades`, `GET /api/v3/trades`, `GET /api/v3/account` | a round trip on a stateful mock exchange: a market buy and a market sell, each reconciled by E1 to E6 (LV2a) | — | synthetic | `production::lv2a_exchange::tests::a_round_trip_passes_every_check_and_writes_the_lines_of_the_spec` | `CexFill`, `OrderState::Filled`, `SpotAccountBalances` |
