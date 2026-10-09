# V7 step 1 — review of V6

Scope: `a914b5b..6eeb651` on `v7-production-validation`: 7 commits, 22 files, +3,160 / −119 lines (the figure
the V7 spec gives). `m10c-balances-resolution` and `origin/main` do not exist as refs in this checkout;
`a914b5b` ("Merge m8b-evm-simulated-node-errors") is the last commit before V6's spec commit (`7617e73`).

Read: every changed source file in full (`src/evm/tx.rs`, `src/evm/rpc.rs`, `src/dex/evm/live.rs`,
`src/dex/mod.rs`, `src/cex/binance/{live,order,balances}.rs`, `src/cex/{orders,stub,mod}.rs`,
`src/balance/*`, `src/testkit/contract.rs`) and the documentation changes (`SPEC.md`, `README.md`,
`IMPLEMENTATION_PLAN.md`, `specs/README.md`) against `specs/V6-balances-and-resolution.md`.

## Baseline

| Check | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --all-targets` | clean |
| `cargo test --lib` | 311 passed, 0 failed, 1 ignored (`binance_spot_testnet`) |
| V6's three `against_anvil_*` tests (`balance/evm.rs`, `evm/tx.rs`, `dex/evm/live.rs`) | **not run**: no `anvil` on this machine, `EVM_ANVIL_RPC_URL` is unset, and the Foundry download is blocked by the session's network policy. They pass as no-ops, as written. V6's own status line says the same ("written and gated, not yet run") |

## Defects

Reported, **not fixed**. Per the instruction for this work, each waits for a decision, then gets a test that
fails first, in its own commit.

### D1 — `EvmLive::resolve` loses the outcome when the landed swap cannot be decoded (confirmed)

`src/dex/evm/live.rs`, `EvmLive::resolve`, the `Ok(Some(outcome))` arm:

```rust
let realised = self.realised(&swap, router, outcome).await?;      // can fail
self.timed_out.lock().unwrap().remove(&tx_hash);                   // only reached on success
```

`self.sender.resolve()` has already consumed the outcome and cleared the sender's latch by then. If
`realised` fails (a successful receipt with no `Transfer` of the output token to the recipient, or none of the
input token out of the payer, the two errors `execute` also returns), `resolve` returns the `Err`, but:

- the `timed_out` entry stays, and is never removed;
- the sender's latch is clear, so a second `resolve(hash)` takes the *"its sender no longer holds it as
  unresolved: it was resolved through the sender directly"* branch, which is false and permanent;
- the doc comment on `resolve` says *"After an `Err` from a failed read, everything is as it was, and the same
  call can be made again."* That holds for a failed read, not for this.

Reproduced with a throwaway test (a timed-out swap, then a mined receipt `status 0x1` with `logs: []`; the test
was discarded, the tree is unchanged):

```text
first : Err(swap landed (tx 0xf9fc…d541) but no ERC-20 Transfer of the output token to the recipient was found in its logs
sender unresolved after first: None
second: Err(swap 0xf9fc…d541 timed out, but its sender no longer holds it as unresolved: it was resolved through the sender directly …
entries kept in timed_out: 1
```

Impact: low to moderate. Needs a swap that landed with logs the decoders reject (a router that pays the
recipient in the native token, a recipient that is not the one in the request). Nothing is sent wrongly and
the sender is free to send again. The cost is a misleading second error and one leaked entry per occurrence,
on the path a consumer takes to reconcile an unknown swap.

Proposed fix (smallest): once `sender.resolve()` has returned an outcome, remove the `timed_out` entry
whatever `realised` returns, and say in the doc that a landed swap that cannot be decoded is an `Err` once, as
from `execute`, and then "nothing to resolve". The alternative, keeping the `TxOutcome` so every later ask
returns the same error, adds state for a case `execute` already answers once.

Proposed failing test: `a_timed_out_swap_that_lands_undecodable_is_an_error_once_and_forgotten`: the probe
above, asserting the second call says "nothing to resolve" and `timed_out` is empty.

**Status: fixed**, on the owner's word, in its own commit. The test above was written first and failed on the
second assertion (the second ask said "resolved through the sender directly"); the entry is now removed as soon
as the sender has handed the outcome over, before the logs are decoded. `resolve`'s doc comment and
`SPEC.md` §5 say that such a swap is an `Err` once and then forgotten.

## Observations (not defects against V6's text; a decision is wanted)

### O1 — `connect` fails open on any JSON-RPC error from `web3_clientVersion`

`src/evm/tx.rs`, `EvmSender::connect`: `Err(err) if err.downcast_ref::<RpcError>().is_some() => Landed`.
Every error *object* is read as "this node has no such method, so it is not anvil", including `-32005` (rate
limit), a gateway's "method not allowed", or any transient error. V6 and `SPEC.md` §5 say exactly that ("a node
that answers with an error object is not anvil"), so the code matches the spec. The cost is the one V6 set out
to remove: a signing sender on an anvil fork behind a rate-limited or filtering gateway reads as `Landed`.
Narrowing to `-32601` (method not found) and treating other error objects as an `Err`, as an unreachable node
already is, would close it, at the price of a connect failure on a provider that answers some other code for an
unsupported method. Needs your call; V7's `production_lv1_base` C4 would show what real providers answer.

**Status: done**, on the owner's word, in its own commit. `connect` now reads `Landed` from an error object only
when its code is `-32601`; any other error object is an `Err` naming `web3_clientVersion`, with the registry slot
left free. Test first: `a_node_that_answers_the_version_with_another_error_refuses_the_connect` (a `-32005` rate
limit) failed before the change. `SPEC.md` §5 and the doc comments say so. This changes V6's stated rule ("an
error object is not anvil") for every code but `-32601`; `a_node_that_has_no_client_version_method_is_not_anvil`
(`-32601`) is unchanged and passes. A provider that answers some other code for an unsupported method will now
refuse the connect: V7's `production_lv1_base` C4 records what real providers answer.

### O2 — `label()` of `EvmLive` and `EvmLiquidity` follows the sender's provenance

A signing sender on anvil is now `Simulated`, so `EvmLive::label()` is `"evm-live-fork"` and
`EvmLiquidity`'s `"evm-liquidity-fork"` for it. V6's text and `SPEC.md` do not mention labels. Nothing in this
crate branches on a label. Consistent with the intent; undocumented.

### O3 — pre-existing, moved by V6, not changed: `at: 0` when the block number cannot be read

`EvmLive::realised`, the `TimedOut` arm: `block_number().await.unwrap_or(0)`. The line is identical before
V6 (it moved from `execute`). A timed-out swap whose block read also fails reports `at = 0`. V7's rule is that a
figure that cannot be computed is an error, not zero; this is a block height, not money, so it is listed and
left alone.

## Checked, no defect

- `NotFound` after the window: `order_state` takes `asked_at` before the query is signed; a `-1021` retry
  inside `signed()` only makes the real ask later, so the error is on the `Open` side. `recv_window_ends` adds
  the clock's error bound. A failed read is an `Err`, never `NotFound` (tested, including a 503 past the window).
- The `lost` map: filled only for an `ApiError::Lost` that ended `OrderStateUnknown` (`downcast_ref` finds it
  through `anyhow` context layers); removed on any found order and on a conclusive `NotFound`; it is not pruned
  for an id the caller never asks about, which V6 documents ("until that order's state is read").
- `state_of`: a non-terminal order is `Open` whatever it has filled; a terminal one with nothing filled is
  `Rejected`; otherwise `Filled` only for `FILLED`, else `PartlyFilled`, with the trade lines read by the code
  `execute` uses.
- `EvmSender::connect`: the client version is read before the registry slot is taken, so a node that cannot be
  asked leaves the slot free (tested). `fork()` is `Simulated` as before. `ensure_balance` still refuses to write
  on a signing sender.
- `TxReplacedOrDropped`: `Display` is the text the error always had; any other `Err` from `EvmSender::resolve`
  leaves the latch (tested).
- `EvmBalances`: a balance above `u128` is an error naming holder and token; a past block the node cannot serve is
  an `Err` (the node's error is passed up, not replaced by `latest`); an empty `eth_call` result is an error
  (`first_word` needs 32 bytes), not zero.
- `SpotBalanceReader` on `BinanceRest`/`BinanceLive`: `free` and `locked` are `Decimal` from the venue's strings;
  `omitZeroBalances=true`; `uid` and permissions are not read.
- Stubs: `EvmBalanceStub` answers from a block history and errors before the first value; `CexStub::order_state`
  never invents a state; the contract suites run against each.
- Documentation against code: `SPEC.md` §5, §6, §6c and §7, `README.md` and `IMPLEMENTATION_PLAN.md` Phase 16
  match the signatures and the behaviour above.

## Facts about the repository that the V7 spec gets slightly wrong (for the steps that follow)

- `BinanceLive::rest` is `pub(crate)`. The V7 spec's "`BinanceLive` gains `pub fn rest(&self)`" means making it
  `pub` (step 4).
- `CommissionDiscount` exists (`src/cex/binance/rest.rs`); so does `CommissionRates` (maker, taker). The spec's
  new `Commission` (maker, taker, buyer, seller) is a different, wider type; both will exist.
- The testnet recording (`REDACTED-UID`, 34 files) is not in this repository or on any branch. Step 2 waits for it.
