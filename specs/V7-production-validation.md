# V7 — a production test tier, and the reads it needs

Status: proposed, by a consumer that is about to send its first real orders and swaps and wants to know,
before any strategy runs, what each venue answers: to a request it must refuse, to a read, and to one
small order or swap at a time. Needs nothing external to **build**: every test it adds has an offline
self-test, and the production tier itself is run by a human, with keys and money the implementer does not
have. Branch `v7-production-validation`, from `m10c-balances-resolution` (V6). About 15 days *(estimate)*.

## Why

**The crate has never been run against a production venue.** `BinanceLive` has one ignored test,
`binance_spot_testnet`, which refuses any host but the testnet's. `EvmLive` has run one swap on Sepolia.
Nothing records what Binance's production host or Base's mainnet nodes answer to a request that is refused,
to an account read, or to a fill. The crate's own error rule (`SPEC.md` §6: an `Err` means nothing filled,
unless it is an `OrderStateUnknown`) is tested on bodies copied from documentation, and six test bodies
still say so: `TODO(R2)` (`src/cex/binance/live.rs`, four; `src/cex/binance/balances.rs`, two).

**A refusal cannot be told apart by a caller.** A refused call's venue code is in the error's text
(`"refused (HTTP 400, code -1013): …"`) and in a `pub(crate)` enum. The existing testnet test finds it with
`format!("{err:#}").contains("-1013")`. A caller that must act differently on `-2010` (no balance), `-2015`
(key, IP or permission) and `-1021` (clock) has nothing typed to read.

**Four reads a production run needs do not exist.** The account's commission rate for a symbol, the key's
restrictions (withdrawals, IP restriction, trading), the top of the book over REST, and a symbol's rules as
a typed read (today a test helper parses `exchangeInfo` by hand). `grep` of `src` finds none of
`account/commission`, `apiRestrictions` or `bookTicker`.

**A landing's timing and position are not read.** `Receipt` carries `gasUsed`, `effectiveGasPrice` and
`l1Fee` but not the transaction's index in its block, and no call reads a block's transaction count.

## The line this draws in `SPEC.md` §2

None. Everything here is a read, a test, or a typed view of an answer the venue already gives. No
adapter's behaviour changes; every existing figure and `Display` string is unchanged (pinned by the
existing tests). Nothing is kept between calls except what a test keeps for itself.

## What this does not do

It does not model a pool, simulate a swap, or know what a consumer does with a reply. A test here may
say *"the answer equals the chain's own record"*; it never says *"the answer equals what my model
predicted"*. That second question needs the consumer's model and stays in the consumer. The spec uses no
consumer vocabulary, and no test may name a consumer type.

**Slippage is recorded here and computed elsewhere.** The difference between the price a caller expected and the
price an order got is split by the consumer into six terms, each a move between two *reference* prices or
amounts. This tier does not compute a term. It records, with a timestamp, every reference that exists at the
venue when a call is made, and the answer the call got, so that the consumer can. The fields are named in
*LV2a* and *LV2b* below, and in the `references` object of `results.jsonl`. A reference that does not exist
is `null`, never zero.

## Contract

All additions are public and additive.

### A typed refusal (`SPEC.md` §6)

```rust
// src/cex/mod.rs
/// The venue read the request and refused it: it did not act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueRefusal {
    pub status: u16,
    /// The venue's own code, when the body carried one (Binance: -1013, -2010, -2015, -1021, …).
    pub code: Option<i64>,
    pub msg: String,
}
impl std::fmt::Display for VenueRefusal { /* exactly the text ApiError::Refused prints today */ }
impl std::error::Error for VenueRefusal {}

pub fn refusal_of(err: &anyhow::Error) -> Option<&VenueRefusal>;   // found with downcast_ref, as `provenance_of` is
```

It is a layer in the error's chain, added wherever an `ApiError::Refused` becomes an `anyhow::Error` (the
order path, `test_order`, `order_state`, the account read, every new read, and the futures client, which
shares `client.rs`). **The error's `Display` is unchanged**, as `ErrorProvenance` leaves it, and
`downcast_ref::<OrderStateUnknown>()` still finds what it found. `Lost` and `NotSent` stay as they are.

### Reads on `BinanceRest` (and reachable from `BinanceLive`)

Inherent methods, as `test_order` is: they are venue-specific, and a second venue would motivate a trait.
Bodies are from Binance's documentation until the production tier records them; each is `documented` in
the catalogue (below) until then. Shapes, from
<https://developers.binance.com/docs/binance-spot-api-docs/rest-api>, to be checked against the docs
before coding.

```rust
// signed; weight 20
pub async fn account_commission(&self, symbol: &str) -> Result<SymbolCommission>;      // GET /api/v3/account/commission
pub struct SymbolCommission { pub symbol: String, pub standard: Commission, pub special: Commission,
                              pub tax: Commission, pub discount: CommissionDiscount }  // CommissionDiscount exists
pub struct Commission { pub maker: Decimal, pub taker: Decimal, pub buyer: Decimal, pub seller: Decimal }

// signed; production host only (the testnet has no /sapi: that answer is itself a recorded case)
pub async fn api_restrictions(&self) -> Result<ApiRestrictions>;                       // GET /sapi/v1/account/apiRestrictions
pub struct ApiRestrictions { pub ip_restrict: bool, pub enable_reading: bool, pub enable_withdrawals: bool,
                             pub enable_internal_transfer: bool, pub enable_spot_and_margin_trading: bool,
                             pub enable_futures: bool, pub enable_margin: bool, pub create_time_ms: Option<u64> }

// public
pub async fn book_ticker(&self, symbol: &str) -> Result<BookTicker>;                   // GET /api/v3/ticker/bookTicker
pub struct BookTicker { pub symbol: String, pub bid_price: Decimal, pub bid_qty: Decimal,
                        pub ask_price: Decimal, pub ask_qty: Decimal }

pub async fn symbol_rules(&self, symbol: &str) -> Result<SymbolRules>;                 // GET /api/v3/exchangeInfo?symbol=
pub struct SymbolRules { pub symbol: String, pub status: String, pub base_asset: String, pub quote_asset: String,
                         pub lot_step: Decimal, pub min_qty: Decimal, pub min_notional: Option<Decimal>,
                         pub apply_min_to_market: Option<bool> }                       // status "TRADING" is the one that trades

pub async fn recent_trades(&self, symbol: &str, limit: u16) -> Result<Vec<PublicTrade>>; // GET /api/v3/trades
pub struct PublicTrade { pub id: u64, pub price: Decimal, pub qty: Decimal, pub time_ms: u64, pub is_buyer_maker: bool }

// the book an order is sent against: its levels, and the id the venue's own stream numbers its updates by
pub async fn order_book(&self, symbol: &str, limit: u16) -> Result<OrderBookSnapshot>;  // GET /api/v3/depth
pub struct OrderBookSnapshot { pub last_update_id: u64, pub bids: Vec<(Decimal, Decimal)>, pub asks: Vec<(Decimal, Decimal)> }  // (price, quantity), best first
```

All money and quantities are `Decimal`, parsed from the venue's strings, never from floats. A field the
venue omits is `None` or a named error, never a zero. `BinanceLive` gains `pub fn rest(&self) -> &BinanceRest`
if it holds one privately.

### Receipt position (`src/evm/rpc.rs`)

```rust
pub struct Receipt { /* existing fields */ pub transaction_index: Option<u64> }        // `transactionIndex`, hex
impl EvmRpc { pub async fn block_transaction_count(&self, block: BlockTag) -> Result<u64>; }  // eth_getBlockTransactionCountByNumber
```

`None` when the node omits the field. Existing receipt tests and constructors are updated for the new
field; nothing else reads it.

## The production tier

In-crate `#[cfg(test)]` modules under `src/production/`, because the signed client is `pub(crate)`. Every
test that touches a production venue is `#[ignore = "…"]`, as `binance_spot_testnet` is, and is run by hand:

```text
cargo test --lib production_lv1_binance -- --ignored --nocapture
```

**Nothing in this spec requires the implementer to run an ignored test.** The implementer's environment
may have no route to Binance (the crate's own README records an HTTP 451 from the build environment) or to
a Base node, and holds no key.

### The harness (`src/production/mod.rs`)

| Part | Rule |
|---|---|
| **Host guard** | Binance spot: the base URL must be exactly `https://api.binance.com`; the testnet, any other host and an unset variable are refused **by name**. A node URL must not be `localhost`, `127.0.0.1` or an anvil (`web3_clientVersion`). The guard takes the allowed host as a parameter so that unit tests can use a mock server; the production constant is the only value an ignored test passes |
| **Spend ledger** | `VP_SPEND_CAP_USD` is required (no default). Every order's notional is checked against `VP_ORDER_CAP_USD` (default 12) **before** it is signed. The ledger refuses the next call once the cumulative loss, valued at the fills' own prices, would exceed the cap. A value the ledger cannot compute stops the run; it is never counted as zero |
| **Halt file** | `VP_HALT_FILE`, required: its presence is checked before every call; present means stop, written as a `halted` verdict |
| **Dry run** | `VP_DRY_RUN=1` prints every call (method, path, parameters; **no key, no signature, no timestamp**) and sends nothing; the call returns `NotSent("dry run")`. Every ignored test supports it |
| **Record mode** | `VP_RECORD_DIR` makes each reply saved as `<step>-<name>.json` with `<step>-<name>.status` beside it (the HTTP status). The only edit is the account's `uid`, replaced by `REDACTED-UID`; **every other byte is unchanged** (the directory is one recording; the run refuses to start if it exists) |
| **Results** | `VP_OUT` names a directory; the run writes `results.jsonl`, one line per call, schema below. A key, a secret, a signature or a `uid` is never written to it |
| **Cases are skipped, never passed** | a case whose keys or inputs are absent is written `skipped` with the reason |
| **Request hooks** | `#[cfg(test)]` helpers on `BinanceClient` that send a signed-endpoint request (a) with an explicit timestamp, (b) with the signature omitted, (c) with the signature corrupted by one byte. None retries on `-1021` (the normal path re-reads the clock and retries once, so B3 cannot otherwise be observed) |

**`results.jsonl`**, one object per call (extra keys allowed; these are the contract with whoever reads it):

```json
{"run_id":"…","case":"B7","venue":"binance-spot","host":"api.binance.com","started_ms":1791395227000,
 "request":{"method":"POST","path":"/api/v3/order","params":{"symbol":"AEROUSDT","side":"BUY","type":"MARKET","quantity":"10"}},
 "outcome":{"kind":"refused","http_status":400,"code":-2010,"msg":"…"},
 "expected":{"kind":"refused","codes":[-2010]},
 "verdict":"pass","reason":"","body_file":"07-order-insufficient-balance.json","latency_ms":212,
 "references":null}
```

A call that trades (LV2a, LV2b) carries a `references` object with the fields of its section's table; every other call has `"references": null`.
`outcome.kind` is `ok`, `refused`, `lost` or `not_sent`. `verdict` is `pass`, `fail`, `skipped`, `halted` or
`unmapped`. **`unmapped`** is a reply whose kind or code is in no row of the catalogue for that case: it is
neither a pass nor a fail, and it fails the run's exit status until a row is added.

### Every case has an offline self-test

For each case below the harness includes a normal (not ignored) test that runs the same case code against
a `wiremock` server standing in for the venue: once with the expected reply (the verdict is `pass`) and
once with a different one (the verdict is `fail` or `unmapped`, and says why). Where a recorded body
exists (`fixtures/`, below) the mock serves it. This is how the implementer proves the case code without
running it.

## The cases

### LV1 — requests that must be refused, and reads

`production_lv1_binance`, one test, cases in order. Keys: `BINANCE_API_KEY` / `BINANCE_API_SECRET` (the
production key: trading enabled, IP-restricted, withdrawals off); optionally `BINANCE_RO_API_KEY` /
`BINANCE_RO_API_SECRET` (a key created with trading disabled) and `BINANCE_EMPTY_API_KEY` /
`BINANCE_EMPTY_API_SECRET` (an empty sub-account). Symbol: `VP_SYMBOL` (default `AEROUSDT`).

| # | Request | How it is provoked | Expected | Self-test body |
|---|---|---|---|---|
| B1 | a signed endpoint with no signature | omit it | refused; the code is recorded (documented: `-1102`) | documented |
| B2 | a wrong signature | alter one byte | refused; the code is recorded (documented: `-1022`) | documented |
| B3 | a timestamp outside `recvWindow` | `signed_with_timestamp` with a timestamp 10 s old | refused `-1021` | documented |
| B4 | a key from a host that is not whitelisted | run the same test from a second host with `VP_EXPECT_NOT_WHITELISTED=1` | refused `-2015` | documented |
| B5 | an order with a key that may not trade | `BINANCE_RO_*` | refused; the code is recorded (documented: `-2015`) | documented |
| B6 | an order under the minimum notional; a quantity off the lot step | $4; half a step | refused `-1013` | `06-order-notional`, `06-order-lot-size-off-step` |
| B7 | **empty wallet**: a market order above the free balance | `BINANCE_EMPTY_*` (free quote balance under `VP_EMPTY_MAX_USD`, default 5, checked first or the case is `skipped`) | refused `-2010` | `07-order-insufficient-balance` |
| B8 | an unknown symbol | a symbol that does not exist | refused (documented: `-1121`) | documented |
| B9 | `test_order` with commission rates, a valid order of about $10 | exists as `BinanceLive::test_order` | accepted; the rates are read | `03-order-test-buy` |
| B10 | `account_commission`, `api_restrictions`, `symbol_rules`, `book_ticker`, `order_book`, `balances` | the new reads | ok; `enable_withdrawals == false`, `ip_restrict == true`, `status == "TRADING"` are **asserted** | `02-account-commission`, `01-account`, `00-exchange-info`; the rest documented |

Safety: every order in LV1 is under `VP_ORDER_CAP_USD` except B7's, which runs only against an account the
case has just read as holding under `VP_EMPTY_MAX_USD`. A refused case that is **accepted** is a
`fail` and the run stops at once.

`production_lv1_base`, run once for each provider in `VP_BASE_RPC_URLS` (comma-separated), with a
**throwaway key** generated for the run (it controls nothing and holds nothing: derive it from a hash of the
time, the process id and a counter; it is never written):

| # | Request | How | Expected | Self-test |
|---|---|---|---|---|
| C1 | an unfunded signer sends | `EvmSender` from the throwaway key sends a zero-value self-transfer | the node refuses; **the provider's wording is recorded** and the error is not a revert | a mock node answering `-32000 insufficient funds …` (exists at `src/evm/tx.rs`) |
| C2 | a call whose estimate reverts | `transfer(…, 1)` of USDC (`VP_TOKEN_QUOTE`) from the throwaway address | `RpcError::is_revert()`; nothing is broadcast; the sender's nonce is unchanged | a mock estimate that reverts |
| C3 | a wrong chain id | `connect` with chain id 1 against a Base node | refused at connect, naming both ids | a mock node |
| C4 | what the provider serves | `eth_getBlockByNumber("pending")`, `eth_call` at `pending`, `eth_maxPriorityFeePerGas`, a log range over the provider's limit; **one request at a time, never a burst** | each reply's shape is recorded; no assertion on the provider | mocks |
| C5 | a dead node | a closed port; a mock that sleeps past the request timeout | an error that is not a revert (`is_revert() == false`). *Not ignored: it is offline* | itself |
| C6 | reads from a pool | `stable()`, `getFee(pool,bool)`-style views, `getReserves()`, `code(pool)` hash, for `VP_POOL` | the raw replies are recorded and decode | mocks |

**Exit of LV1.** Every case's verdict is `pass` or `skipped` with a reason; no `unmapped`; no `fail`; every
reply is recorded; the account's balances and the throwaway address's are unchanged to the unit (read before
and after with `SpotBalanceReader` and `EvmBalanceReader`).

### LV2a — one exchange order at a time, reconciled to the unit

`production_lv2a_exchange`. Ten round trips on `VP_SYMBOL` (a market buy, then a market sell of what the
buy delivered), each of about `VP_ORDER_USD` (default 10, within the cap and above the symbol's minimum
notional): one at about $6, one whose quantity needs rounding to the lot step. The account is the
production key's. Before each order the test reads `order_book(symbol, 5)` and the balances; after it, the balances.

**EXACT checks** (any failure is a `fail` and stops the run):

| # | Check |
|---|---|
| E1 | `order_state(symbol, client_order_id)` equals the fill `execute` returned: status, executed quantity, trades |
| E2 | the order's `myTrades` lines equal `fill.trades`: ids, prices, quantities, commissions, commission asset |
| E3 | every `trade_id` is found in `recent_trades(symbol, 1000)` with the same price and quantity; if the window no longer holds it the check is `skipped_window`, never a pass |
| E4 | when the commission asset is the asset received, commission = Σ quantity × the `account_commission` rate for that side, to the asset's scale; otherwise the rate and asset are recorded and the check is `skipped_asset` |
| E5 | the account's free + locked change, per asset, equals the signed fill: ± quantity, ∓ Σ price × quantity, − commission, exactly |
| E6 | `filled_qty` is a whole number of lot steps; `fill.venue_time_ms` is the venue's `transactTime` |

**References and measurements** (recorded in the row; no assertion, a band is the consumer's). Just before each
send the test reads `order_book(symbol, 5)` and notes the local time before the request and after the answer.
After the fill it records:

| Field | What it is |
|---|---|
| `touch_at_send` | the best bid and ask of that snapshot, its `last_update_id`, the local times around the request |
| `sent_ns`, `returned_ns` | the local clock immediately before the signed order left and when its answer returned |
| `transact_time_ms` | the venue's own `transactTime` (or `updateTime` from a status query) |
| `avg_fill` | the volume-weighted price of the trade lines, and each line's id, price, quantity, commission and asset |
| `depth_walked` | the levels of the snapshot the filled quantity would have used, computed from the snapshot |
| `rate` | the taker rate `account_commission` gave for the side, and the asset the commission was paid in |

These are the references of the consumer's terms 5 (touch at send → average fill) and 6 (commission). Term 4 needs a
decision's booked touch; this tier has no decision, so it is `null`. A round trip costs about 2 × the commission rate
of $10 plus the spread; the ledger enforces the cap.

### LV2b — one swap at a time, the port's reading equals the chain's record

`production_lv2b_chain`. Sixty swaps of about `VP_SWAP_USD` (default 10) through an EVM router, alternating
direction, at exponential intervals (mean 90 s), at the node's suggested priority fee, through a shared
`EvmSender` and `EvmLive`. Inputs: `VP_BASE_RPC_URL`, `VP_SIGNER_KEY_HEX` *(read once from the environment of the shell that
starts the run, removed from the process environment after reading, never logged, never a file in the
repository; no keystore reader is added, so no dependency is)*, `VP_ROUTER`, `VP_POOL`, `VP_TOKEN_IN`,
`VP_TOKEN_OUT`, `VP_NATIVE_USD`. The calldata is built in the test with `alloy_sol_types::sol!`: for
Aerodrome, `swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, (address from, address to,
bool stable, address factory)[] routes, address to, uint256 deadline)`, with the factory read from the pool
(`factory()`), the deadline 60 s ahead, and `amountOutMin` 30 bps under `getAmountsOut` read at `pending`.
*(Check the router's verified ABI before coding; if it differs, stop and say so in `V7-questions.md`.)*
Approvals are exact-amount, as `EvmLive` already makes them.

**EXACT checks**, against an independent decoding of the raw `eth_getTransactionReceipt` JSON written in the
test (the logs' `Transfer(address,address,uint256)` events), not against the adapter's own code:

| # | Check |
|---|---|
| X1 | `Realised.amount_out` equals the sum of the output token's `Transfer`s to the recipient |
| X2 | `Realised.amount_in` equals the input token's `Transfer`s out of the payer, less any back to it |
| X3 | `cost.gas_used`, `cost.effective_gas_price_wei` and `cost.l1_fee_wei` equal the receipt's `gasUsed`, `effectiveGasPrice`, `l1Fee` |
| X4 | the signer's native balance change between block *inclusion − 1* and *inclusion*, read by `EvmBalanceReader`, equals `gasUsed × effectiveGasPrice + l1Fee` exactly. **This is the first measurement of a real L1 fee** |
| X5 | the signer's token balance changes equal ± the two amounts |
| X6 | `Realised.at` is the inclusion block; `tx_ref` is the transaction's hash; the sender's nonce advanced by one |

**References and measurements** (recorded, no assertion). Before each send the test reads the quote
(`getAmountsOut` at `pending`) and the block number it was read at. After the swap it records:

| Field | What it is |
|---|---|
| `quote_pre_send` | the amount out at `pending`, the block it was read at, the local time |
| `calldata`, `router`, `token_in`, `token_out`, `amount_in` | what is needed to run the same swap again at another block (the consumer re-simulates it at the top of the landing block) |
| `sent_ns`, `tx_hash` | the local clock before the signed transaction left; its hash |
| `first_seen_ns`, `sealed_ns` | the first time `eth_getTransactionReceipt` returns the transaction (polled every 50 ms with `set_poll_settings`) and the first time `eth_blockNumber` is at or past its block, so a *preconfirmed* receipt and a *sealed* one are told apart |
| `inclusion_block`, `transaction_index`, `block_tx_count` | where it landed |
| `transfer_log_indices` | the `logIndex` of each of the transaction's `Transfer` logs, so the consumer can name the pool's swaps ahead of it in the block |
| `amount_in_taken`, `amount_out` | what `Realised` says, beside the independent decoding of X1 and X2 |
| `gas_used`, `effective_gas_price_wei`, `l1_fee_wei` | the receipt's cost |
| `amount_out_vs_quote_bps` | `amount_out` against `quote_pre_send`, in bps of the mid |
| `revert` | a swap's revert, if any, with its reason |

These are the references of the consumer's terms 2 (quote → top of the landing block), 3 (top of the landing block →
returned) and 6 (gas and the L1 fee). Term 1 needs a decision block's simulation, so it is `null`. The pool's own
events (`Swap`, `Sync`) are not decoded here: that is pool knowledge, and the consumer reads them from its own feed
or from `eth_getLogs` at the inclusion block.

**Stops:** cumulative loss over `VP_SPEND_CAP_USD` (the wallet valued at the pool's own price and
`VP_NATIVE_USD`); any EXACT failure; more than 10 % of swaps reverting; a landing five or more blocks after
the send.

**Self-test:** the checks run against a `wiremock` node serving a receipt built in the test; each check has a
mutation (one field changed) that makes it fail. A further test asserts that every field of the table above is
present in a swap's `references` object, and `null`, not absent or zero, where the reference does not exist.

## The response catalogue and the recorded bodies

**Recorded bodies** live at `fixtures/<venue>/<origin>/<yyyy-mm-dd>/`. This spec's first step copies the
consumer's recording of Binance's spot testnet (7 October 2026, 34 files, 164 KB, the account's `uid`
already replaced by `REDACTED-UID`) to `fixtures/binance-spot/testnet/2026-10-07/`, with its README. A
production run's `VP_RECORD_DIR` is copied to `fixtures/binance-spot/production/<date>/` by hand, after
reading it.

**The catalogue** is `docs/responses/binance-spot.md` and `docs/responses/evm.md`: one row for each
**call × response variant**, columns: the call · the variant · the body (a path under `fixtures/`) · its
**origin** (`production`, `testnet`, `documented`, `synthetic`, `not provokable`) · the test that parses it ·
the typed result in this crate (`CexFill`, `OrderStateUnknown`, `VenueRefusal{code}`, …). It is **not**
called `provenance`: `Provenance` already means `Simulated` or `Landed`. A cell whose origin is
`documented`, `synthetic` or `not provokable` stays in the table: the gap is visible. The consumer adds its
own columns in its own repository.

**One test keeps the catalogue honest**, `catalogue_matches_fixtures`: every file under `fixtures/` has a
row, every row's path exists, and every row names a test that exists and reads that body. The `unmapped`
verdict above is the same rule at run time.

**Not provoked, on purpose** (rows say `not provokable` or `documented`): a `418` ban and a `429` storm; a
repeated client order id after a fill (Binance accepts it, so proving it would trade twice); a partial fill
or a market order beyond the book; a mined revert (the signing sender refuses to broadcast a call whose
estimate reverts, so a real revert receipt is fetched from chain history instead, read-only); a dropped or
replaced transaction.

## Order of work

Each step compiled and tested before the next. Offline tests only.

1. **Branch** `v7-production-validation` from `m10c-balances-resolution`. Review that branch's diff against
   `origin/main` (22 files, about 3,160 lines) and write findings to `specs/V7-review-of-V6.md`; fix a
   defect only with a test that fails first, in its own commit. *(1 day)*
2. **Recorded bodies.** Copy the testnet recording to `fixtures/`; replace the six `TODO(R2)` bodies by
   `include_str!` of the recorded ones; where a parser disagrees with a recorded body, fix the parser and
   say so. Add `catalogue_matches_fixtures` and the first catalogue rows. *(1½ days)*
3. **`VenueRefusal`** and `refusal_of`, attached everywhere a refusal becomes an error; tests for each path
   and for `Display` being unchanged. *(1 day)*
4. **The new reads** with `wiremock` tests on the documented bodies, and `Receipt.transaction_index` /
   `block_transaction_count`. *(2 days)*
5. **The harness**: guard, ledger, halt file, dry run, record mode, results writer, clock hook, with
   offline tests for each rule (the guard refuses the testnet, a mock host and an unset variable; the ledger
   refuses the order that would pass the cap; record mode leaves every byte but the `uid` unchanged; dry run
   sends nothing, asserted by the mock receiving zero requests). *(2½ days)*
6. **LV1** cases and their self-tests. *(2½ days)*
7. **LV2a** and its self-test. *(2 days)*
8. **LV2b** and its self-test. *(2½ days)*
9. **Docs**: `SPEC.md` §7's table gets the production tier, §6 the typed refusal, a new §6d the reads;
   `README.md` status rows; `specs/README.md` row V7; `IMPLEMENTATION_PLAN.md` Phase 17; the catalogue.
   *(½ day)*

**Done when:** `cargo fmt --check`, `cargo clippy --all-targets` and `cargo test` (offline) are clean and
every figure that was in the suite before is unchanged; every case above has a passing and a failing
self-test; `cargo test --lib production -- --ignored --list` lists `production_lv1_binance`,
`production_lv1_base`, `production_lv2a_exchange` and `production_lv2b_chain`; `DRY_RUN=1` of each prints its
calls and sends none; the catalogue test passes; the six `TODO(R2)` markers are gone.

**Status when built:** written, offline-tested, **not run against a production venue**. The README row says
exactly that.

## Notes for the implementer

- **Do not run an ignored test.** There is no key, no money, and probably no route. Do not add a key, a
  token, an address you did not read from the task's inputs, or a default for a credential, anywhere.
- **Do not push to `main`.** Push the branch; open the pull request against `m10c-balances-resolution`'s
  base only when asked.
- **Do not change behaviour.** An existing test that changes is a stop: write the question to
  `specs/V7-questions.md` and continue with what does not depend on it.
- **Money and quantities are `Decimal` or integers.** Never a float. A figure that cannot be computed is an
  error, not zero.
- **Commit messages state the technical change only**, no attribution line of any kind.
- If the documented body of a read differs from what is above, follow the documentation, record the
  difference in the catalogue, and keep the typed shape.
