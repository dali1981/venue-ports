# V7 — handover for the remaining work

For a fresh remote session (or a person) picking up `specs/V7-production-validation.md` on branch
`v7-production-validation`. Read this file, then the spec, then `specs/V7-review-of-V6.md`. Written after step 4
(commit `122f61f`) and updated after step 6 (commit `033f8e1`, see §9, which is the part to read first); the branch is
pushed and is the only copy of the work.

## 1. Where things stand

| Spec step | State | Commits |
|---|---|---|
| 1. Branch, review of V6 | **Done.** One defect (D1) found, then fixed on the owner's word, test first, own commit. Two observations (O1, O2) done on the owner's word | `f5f4ddb` review, `d80bd8a` D1, `1f7a927` O1, `f714c6e` O2 |
| 2. Recorded bodies | **Not started.** The testnet recording is not in the repo (see §4) | — |
| 3. `VenueRefusal`, `refusal_of` | **Done** | `5f9979d` |
| 4. The new reads, receipt position | **Done** | `740323f` (EVM), `122f61f` (Binance) |
| 5. The harness | **Done** | `924e693` |
| 6. LV1 cases | **Done** (mutation-checked for the harness only, see §9.4) | `033f8e1` |
| 7. LV2a | **To do** | — |
| 8. LV2b | **To do** | — |
| 9. Docs | **To do** (nothing of step 9 is written yet, see §6) | — |

Baseline to confirm before touching anything: `cargo fmt --check`, `cargo clippy --all-targets` clean, and
`cargo test --lib` gave **359 passed, 0 failed, 1 ignored** (`binance_spot_testnet`) before step 5; it now gives
**496 passed, 0 failed, 3 ignored** (the other two are `production_lv1_binance` and `production_lv1_base`). The first build takes about
two minutes, then the suite takes about five seconds.

Not yet in the repo: `src/production/`, `fixtures/`, `docs/responses/`, `specs/V7-questions.md`. Six `TODO(R2)`
markers remain (`src/cex/binance/live.rs` four, `src/cex/binance/balances.rs` two).

## 2. What this environment can and cannot do

Learned in the session that did steps 1 to 4; check again, a new session may differ.

- Cargo works: crates download through the proxy; no `anvil`, so the V6 `against_anvil_*` tests are no-ops here.
- No route to `api.binance.com`; the spot testnet answers HTTP 451. `developers.binance.com` does not resolve.
  `github.com` release downloads and `api.github.com` are refused. **`raw.githubusercontent.com` works**, which is
  how Binance's public docs were read: `https://raw.githubusercontent.com/binance/binance-spot-api-docs/master/`
  `{rest-api,errors,enums,filters}.md`. The Wallet (`/sapi`) docs are not in that repository.
- No keys, no money. **Never run an `--ignored` test, and never add a key, token or credential default.** Every
  production test is written, offline-tested against `wiremock`, and run by a person.
- The edit tool needs a file read first, and `cargo fmt` rewrites files under it: re-read after formatting.

## 3. Working agreement (what the owner asked for, in order)

1. **Report a defect before fixing it.** Write it down (`specs/V7-review-of-V6.md` for V6 findings; chat for new
   ones) with evidence, wait for the owner, then fix with a **test that fails first**, in **its own commit**.
2. Each spec step is compiled and tested before the next. Run `cargo fmt`, `cargo fmt --check`,
   `cargo clippy --all-targets` and `cargo test` before every push; push to `v7-production-validation` only; **no
   pull request** unless asked; never push to `main`.
3. **Commit messages state the technical change only, no attribution line** (the spec's rule; the session harness
   suggests attribution lines, and the owner was told once that the spec's rule was followed). If the owner says
   otherwise, follow the owner.
4. **Do not change behaviour.** An existing test that has to change is a stop: write the question to
   `specs/V7-questions.md` and carry on with what does not depend on it. (Two assertions in `client.rs` were adapted
   in step 3 for a variant that changed shape, and nothing else.)
5. Money and quantities are `Decimal` or integers, never floats; a figure that cannot be computed is an error, not zero.
6. Habit worth keeping: after writing tests, break the thing they test (one line, restore from a copy) and confirm
   they fail. In steps 3 and 4 it was done for every wiring point and every mutation was caught by the intended test;
   it takes a minute.
7. Say plainly what was not run. Status of this work when finished: *written, offline-tested, not run against a
   production venue.*

## 4. Open decisions for the owner

1. **Is step 2 (the testnet recording) needed to go live? Recommendation: no.** What it buys is (a) the six `TODO(R2)`
   bodies replaced by recorded ones, so the parsers are checked against real bytes, and (b) recorded self-test bodies
   for cases B6, B7, B9, B10. Neither is needed to *run* the production tier, which records production bodies itself
   (`VP_RECORD_DIR`), and those same bodies can later replace the `TODO(R2)` ones (same endpoints: account, order,
   `myTrades`, error payloads). The case for having it: documentation alone missed a real quirk once (the testnet
   answered `"discountAsset": null`, which `CommissionDiscount` now handles), and the recording already exists, so it is
   free regression protection. If the owner keeps step 2: they commit the 34 files to
   `fixtures/binance-spot/testnet/2026-10-07/` (with the README), or hand them to the session; a session cannot
   fetch them. If not: amend the spec's "Done when" (the six `TODO(R2)` markers gone) to say they go after the first
   production recording. Part of step 2 does not need the files: the `catalogue_matches_fixtures` test and the
   catalogue skeleton (rows whose body is `documented`/`synthetic` have no path, which the spec allows).
2. **`SymbolCommission.special` is required** (spec and documentation agree). The existing `CommissionRates` treats
   the same field as optional ("absent from older answers"). If a real answer omits it, `account_commission` fails with
   a missing-field error and E4 in LV2a cannot read a rate. The first recording settles it; changing it to `Option` is
   a one-line edit.
3. **Bybit has no `VenueRefusal`.** `BybitLive` has its own client and error type; the spec names Binance and the
   futures client only. A small follow-up if wanted.
4. **`recent_trades` and `order_book` refuse a limit out of range locally** (1–1000, 1–5000) although the venue
   clamps; easy to relax.
5. O1 changed V6's rule: `EvmSender::connect` now reads `Landed` only from `-32601` ("method not found"); any other
   error object from `web3_clientVersion` refuses the connect. Relevant to LV1 case C4: a provider that answers
   another code for an unsupported method will refuse to connect, and that wording is exactly what C4 should record.

## 5. What was built, for orientation

| Where | What |
|---|---|
| `src/cex/mod.rs` | `VenueRefusal { status, code, msg }`, `refusal_of(&anyhow::Error)`; `VenueRefusal::because` (crate-private, for the order path) |
| `src/cex/binance/client.rs` | `ApiError::Refused(VenueRefusal)`. **`ApiError` is not a `std::error::Error`**: it converts to `anyhow::Error` through one `From`, so a call that forgets to convert does not compile. `ApiError::because(message)` keeps `to_string()` and the refusal under it |
| `src/cex/binance/reads.rs` | `account_commission`, `api_restrictions`, `book_ticker`, `symbol_rules`, `recent_trades`, `order_book` on `BinanceRest`; strict `decimal` deserializer (a JSON number is an error) |
| `src/cex/binance/live.rs` | `BinanceLive::rest()` is public |
| `src/evm/rpc.rs` | `Receipt.transaction_index`, `EvmRpc::block_transaction_count` |
| `src/evm/tx.rs` | `METHOD_NOT_FOUND`; `connect_to_a_node_that_reports(version)` test helper (`pub(crate)`) |
| `src/dex/evm/live.rs` | `EvmLive::resolve` forgets a swap as soon as the sender hands its outcome over |

Bodies used by the step 4 tests: commission, `bookTicker`, trades and depth are **copied from the documentation**;
the `apiRestrictions` body and the whole `exchangeInfo` answer are **synthetic** (assembled from the documented field
names, because the Wallet page was unreachable and the documentation prints no whole `exchangeInfo`). The catalogue
must say `synthetic` for those two. If a session can read the Wallet page, replace the synthetic `apiRestrictions`
body with the documented one.

## 6. What remains, step by step

Each step: do it, offline tests only, commit, push, tell the owner what was and was not run.

### Step 5 — the harness (`src/production/mod.rs`, `#[cfg(test)] mod production;` in `src/lib.rs`)

The spec's harness table is the contract; the points below are what the code will force.

- **Request hooks live in `client.rs`** under `#[cfg(test)]`, because `send` and `sign` are private. Three helpers on
  `BinanceClient`: a signed request with an explicit timestamp; the same with the signature omitted; the same with
  one byte of the signature changed. Build the query with `sign::signed_query(secret, params, recv_window_ms,
  timestamp)` and call `send` directly. None may use `signed()`, which resyncs the clock and retries once on `-1021`
  (that is why B3 cannot be observed otherwise).
- **Record mode needs the raw reply.** `send` reads `response.text()` and parses it; the status and bytes are not
  kept. Add a test-only tap that sees `(path, status, body)` before parsing. "Every other byte unchanged": redact the
  `uid` by replacing the value token after `"uid":` with `"REDACTED-UID"` and nothing else (the original is a JSON
  number, so the result is a JSON string; the account parser ignores `uid`). The run refuses to start if
  `VP_RECORD_DIR` exists. Files are `<step>-<name>.json` plus `<step>-<name>.status`.
- **Dry run** is switched by `VP_DRY_RUN=1` (the spec's harness table; its "Done when" line writes `DRY_RUN=1`, a
  typo to settle with the owner). It is `NotSent("dry run")` (`ApiError::NotSent(anyhow!("dry run"))`); it prints method, path and
  parameters, never the key, signature or timestamp. The test asserts the mock received zero requests.
- **Host guard** takes the allowed host as a parameter so tests can use a mock; the production constant is
  `https://api.binance.com`, and the testnet, any other host and an unset variable are refused by name. A node URL
  must not be `localhost`, `127.0.0.1` or an anvil (`web3_clientVersion` starts with `anvil`).
- **Spend ledger** in `Decimal`: `VP_SPEND_CAP_USD` required, no default; `VP_ORDER_CAP_USD` default 12, checked
  before the order is signed; the next call is refused once cumulative loss, valued at the fills' own prices, would
  pass the cap; a value the ledger cannot compute stops the run.
- **Halt file** (`VP_HALT_FILE`, required) is checked before every call; present means stop, written as `halted`.
- **Results**: `results.jsonl` in `VP_OUT`, one object per call, the schema in the spec. Never write a key, secret,
  signature or `uid`. `verdict` is `pass`, `fail`, `skipped`, `halted` or `unmapped`; `unmapped` fails the run.
- Offline tests: guard refuses testnet, a mock host and an unset variable; ledger refuses the order that would pass
  the cap; record mode leaves every byte but the `uid` unchanged; dry run sends nothing.

### Step 6 — LV1 (`production_lv1_binance`, `production_lv1_base`)

- One case abstraction (id, request, how provoked, expected outcome kinds and codes, self-test bodies) serves B1–B10
  and C1–C6. For every case the harness has a normal test that runs the same case code against `wiremock` once with the
  expected reply (verdict `pass`) and once with a different one (`fail` or `unmapped`, with the reason).
- The new reads (step 4) are B10's subject; `symbol_rules.status == "TRADING"`, `api_restrictions.enable_withdrawals
  == false` and `ip_restrict == true` are asserted.
- B7 runs only after the case reads the empty account's free quote balance as under `VP_EMPTY_MAX_USD` (default 5);
  otherwise it is `skipped` with the reason. A refused case that is *accepted* is a `fail` and stops the run at once.
- `production_lv1_base`: a throwaway signing key derived from a hash of time, process id and a counter, never
  written. C5 (a dead node) is **not** ignored. C1 expects the node to refuse an unfunded zero-value self-transfer,
  with the provider's wording recorded and the error not a revert. See open decision 5 for C4.
- Exit of LV1: every case `pass` or `skipped` with a reason, none `unmapped` or `fail`, and balances before and after
  equal to the unit (`SpotBalanceReader`, `EvmBalanceReader`).

### Step 7 — LV2a (`production_lv2a_exchange`)

Ten round trips; E1–E6 are exact checks (any failure is a `fail` and stops), and the table of references and
measurements is recorded, not asserted. Use `order_state`, `myTrades` through the existing trade-line code,
`recent_trades(symbol, 1000)` (`skipped_window` when the window no longer holds a trade, never a pass),
`account_commission` for E4 (`skipped_asset` when the commission asset is not the asset received), and
`SpotBalanceReader` before and after for E5. `references` is a JSON object on trading calls and `null` otherwise; a
reference that does not exist is `null`, never zero.

### Step 8 — LV2b (`production_lv2b_chain`)

Sixty swaps; X1–X6 are checked against an **independent decoding of the raw receipt JSON written in the test**, not
the adapter's code. `VP_SIGNER_KEY_HEX` is read once and removed from the process environment, never logged or
written. The Aerodrome calldata is built with `alloy_sol_types::sol!`; the spec says to check the router's verified
ABI first and, if it differs, stop and write the difference to `specs/V7-questions.md`. **That check needs a block
explorer, which this environment cannot reach**, so build the calldata in one small isolated function and put the
question in `V7-questions.md` regardless. Self-test: a `wiremock` node serving a receipt built in the test, one
mutation per check that makes it fail, and a test that every field of the references table is present in a swap's
`references` (and `null`, not absent or zero, where it does not exist). Use `unique_test_signer()` or a distinct key
per test: `EvmSender::connect` keeps a per-process registry of (address, chain) and two tests with one key collide.

### Step 2 — recorded bodies (only if the owner keeps it; see open decision 1)

Copy the recording to `fixtures/binance-spot/testnet/2026-10-07/` with its README; replace the six `TODO(R2)`
constants by `include_str!` of the recorded bodies; where a parser disagrees with a recorded body, fix the parser and
say so (a defect: report first). Add `catalogue_matches_fixtures`.

### Step 9 — documents (none written yet)

`SPEC.md` §6 gets the typed refusal (type, `refusal_of`, the layers it rides under, `Display` unchanged, `ApiError`
not being an `Error`), a new §6d gets the six reads, §7's table gets the production tier; `README.md` status rows;
`specs/README.md` already has the V7 row; `IMPLEMENTATION_PLAN.md` Phase 17; `docs/responses/binance-spot.md` and
`docs/responses/evm.md`, one row per call × response variant with origin `production`, `testnet`, `documented`,
`synthetic` or `not provokable` (not called "provenance"), and `catalogue_matches_fixtures` keeping them honest.
`SPEC.md` was edited early only where a behaviour changed (D1, O1, O2).

## 7. For a person, not a session

- Provide the testnet recording if step 2 is kept (open decision 1).
- Run the ignored tests, with keys and money, from a machine with a route to `api.binance.com` and a Base node:
  `cargo test --lib production_lv1_binance -- --ignored --nocapture`, then `production_lv1_base`,
  `production_lv2a_exchange`, `production_lv2b_chain`. Set `VP_DRY_RUN=1` first. Environment: `BINANCE_API_KEY`,
  `BINANCE_API_SECRET`, optionally `BINANCE_RO_*` and `BINANCE_EMPTY_*`, `VP_SYMBOL`, `VP_SPEND_CAP_USD`,
  `VP_ORDER_CAP_USD`, `VP_HALT_FILE`, `VP_OUT`, `VP_RECORD_DIR`, `VP_BASE_RPC_URL(S)`, `VP_SIGNER_KEY_HEX`,
  `VP_ROUTER`, `VP_POOL`, `VP_TOKEN_IN`, `VP_TOKEN_OUT`, `VP_TOKEN_QUOTE`, `VP_NATIVE_USD` (the spec lists them).
- Read a `VP_RECORD_DIR` before copying it to `fixtures/binance-spot/production/<date>/`, and commit it.
- Check the Aerodrome router's verified ABI against `specs/V7-questions.md`.

## 8. Done when (from the spec, with the current state)

`cargo fmt --check`, `cargo clippy --all-targets` and `cargo test` clean, every earlier figure unchanged (359 today);
every case has a passing and a failing self-test; `cargo test --lib production -- --ignored --list` lists
`production_lv1_binance`, `production_lv1_base`, `production_lv2a_exchange` and `production_lv2b_chain`; a dry run of
each prints its calls and sends none; `catalogue_matches_fixtures` passes; the six `TODO(R2)` markers are gone (see
open decision 1).

## 9. Update after step 6 (read this first)

Commit after every step; this session was told to stop at step 6 because its context was tight. Steps 7, 8 and 9
remain, and step 2 waits on the owner (§4.1).

### 9.1 What exists now (`src/production/`, all `#[cfg(test)]`)

| File | What |
|---|---|
| `mod.rs` | `Run`: `start(env, venue, host, echo)`, `binance_rest(env, allowed_host, prefix, timings)`, `authorise_order(qty, price) -> OrderPermit`, `permit_rpc(method, params)` (the gate for a call that is not Binance REST), `call` / `call_with(spec, make, references, check)`, `note` / `note_at` (a line for a case that made no call), `hide_urls`, `finish()` (Err unless no fail, halted or unmapped). `CallSpec::new(case, Expected).record_as(stem).request(..).host(..)` |
| `gate.rs`, `wire.rs` | the seam into `BinanceClient` (halt file, dry run, "no order without the ledger's say-so", record mode) |
| `ledger.rs` | caps and loss in `Decimal`: `authorise(notional)`, `record_fill(&Fill)`, `record_loss`, `stop` |
| `record.rs`, `results.rs`, `env.rs`, `guard.rs`, `sizing.rs` | recording and `uid` redaction; the results schema, `judge`, `Outcome::of_error`; `Env`/`MapEnv`/`ProcessEnv`; host and node guards; lot-step arithmetic |
| `lv1_binance.rs`, `lv1_base.rs` | the LV1 cases and their self-tests; the two ignored tests |
| `testing.rs` | `Setup` (a run in a scratch dir), `fast()`, `MockNode` (a JSON-RPC mock: `result`, `error`, `error_when`, `sequence`, `eth_call` by selector, `asked()`) |
| `cex/binance/client/hooks.rs` | the three request hooks, `with_wire`, `gate`, `observe` |

Reuse these for steps 7 and 8; the patterns to copy are `lv1_binance.rs` (a `MiniVenue` wiremock `Respond` that checks
what the venue checks, `play(case)`, one test per case plus a failing variant) and `lv1_base.rs` (`Lab`, `base_node()`).

### 9.2 Decisions made in steps 5 and 6 that the owner has not seen (flag them)

1. **Ledger rule.** An order is refused when the loss so far is **at** the spend cap, and when *loss + this order's whole
   notional* would pass it (the spec's "refuses the order that would pass the cap"). A cap below one order plus the run's
   losses stops the run early. Reads are never refused. An unvalued commission asset (BNB) or non-USD quote stops the run.
2. **B5 sends an order *test*** (`POST /order/test`) with the read-only key, not an order, so a key that can trade by
   mistake places nothing. B6's lot-step order is *k steps + half a step* sized to the order's notional, not half a step on
   its own (that would also fail `minQty`). B6's notional order is 0.8 of the symbol's `minNotional`, and is skipped unless
   the venue says the minimum applies to market orders.
3. **Second host.** With `VP_EXPECT_NOT_WHITELISTED=1` only B4 runs; without it B4 is skipped.
4. **A dry run makes no read**, so a case sized from a read (every order) is skipped with that reason; the reads and the
   requests that need none are printed. `VP_DRY_RUN=1` is the variable (the spec's "Done when" writes `DRY_RUN=1`: a typo).
5. **Base.** Beyond C1 to C6 there is a `GUARD` case (`web3_clientVersion`: not anvil; `-32601` counts as not anvil; any
   other refusal stops the run, since the node could not be read) and an `EXIT` case (the throwaway address's native and
   quote balances read before and after, equal to the unit). C4 reads the latest block as its own step (`37-latest-block`).
   A line names a provider by its host only; `Run::hide_urls` scrubs the URL, its path and query from every line.
6. **Recording names.** Binance: `00-exchange-info`, `01-account`, `02-account-commission`, `03-order-test-buy`,
   `06-order-notional`, `06-order-lot-size-off-step`, `07-empty-account`, `07-order-insufficient-balance`,
   `08-no-signature` … `17-account-after` (the first seven keep the numbers of the consumer's testnet recording). Base:
   `p<provider index>-3x-…` and `p<n>-4x-…`. A call that makes several requests saves `<stem>.json`, `<stem>-2.json`, …;
   the venue's clock replies are not saved.
7. `cex::binance` became `pub(crate)` (was private) so the tier can reach `client`/`clock`. The public surface is unchanged.

### 9.3 What remains

- **Step 7, LV2a** (`production_lv2a_exchange`, spec "LV2a"): ten round trips; E1 to E6 exact, references recorded. Use
  `BinanceLive::execute` + `order_state`, `rest.order_book(symbol, 5)` before each send, `recent_trades(symbol, 1000)`,
  `account_commission`, `SpotBalanceReader` before and after. **Book every fill in the ledger** (`ledger.record_fill`)
  and call `run.authorise_order` before each order. `call_with`'s `references` closure builds the `references` object,
  `check` the exact checks; `null` where a reference does not exist, never zero. Needs a stateful mini venue (a fill
  changes balances); extend `MiniVenue` or write a sibling in `testing.rs`.
- **Step 8, LV2b** (`production_lv2b_chain`): sixty swaps; X1 to X6 against an independent decoding of the raw receipt JSON;
  `VP_SIGNER_KEY_HEX` via `Env::take` (already removes it from the process). The Aerodrome calldata goes in one small
  function (`sol!`), and **`specs/V7-questions.md` must be created** with the router-ABI question (no explorer here). Use
  `unique_test_signer()`-style distinct keys per test. `MockNode` serves a receipt built in the test; one mutation per check.
- **Step 9, docs**: as §6 above, plus the catalogue (`docs/responses/*.md`, `catalogue_matches_fixtures`). The two
  documented-vs-synthetic notes of §5 still apply. Record in the catalogue: the mini venue's refusal bodies are the
  documented codes and messages (`errors.md`), and the **HTTP status of each (`401` for `-2015` and `-2014`, `400` for the
  rest) is assumed, not documented**.
- **`specs/V7-questions.md`** (does not exist yet) should hold: the Aerodrome router ABI; the pool and factory ABIs C6 uses
  (`factory()`, `stable()`, `getReserves()`, `PoolFactory.getFee(address,bool)`; the selectors are pinned by a test but the
  ABI is from memory, not read from a verified contract); decisions 1 and 2 above; the `DRY_RUN` typo.

### 9.4 What was not done, and traps

- **No mutation check of the LV1 case code.** The harness was broken 27 ways and each was caught (§3.6 habit); the cases of
  step 6 were not. Do it before trusting them: e.g. B3's `- 10_000`, B6's `* 0.8`, B6's `+ lot_step / 2`, `book_accepted`,
  the exit comparison, C1's `connected` flag, C2's nonce check, C3's chain id, C4's gate on the block number. A script that
  applies one textual mutation, runs `cargo test --lib <filter>` and restores the file is in the history of this branch's
  session only; it is twenty lines. **Use the right filter**: the hooks' tests are under `cex::binance::client::hooks`, not
  `production`.
- **A text patch after `cargo fmt` can silently not apply** (fmt reflows the line you are matching). This happened three
  times in step 6 and each looked like a code bug. After a scripted edit, `grep` that it landed.
- **Do not wait with `pgrep -f <name>` in a loop**: the loop's own command line matches. Do not background a command twice
  (`run_in_background` and `&`).
- Nothing in steps 5 and 6 was run against a venue; the status line stays *written, offline-tested, not run against a
  production venue*.
