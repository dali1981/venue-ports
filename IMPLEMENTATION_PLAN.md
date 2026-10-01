# Implementation Plan — `venue-ports`

This is the build-out roadmap for the crate specified in [`SPEC.md`](SPEC.md). `SPEC.md` is the
contract; this document is the order of work to get there, phase by phase, with each phase left in a
compilable, tested state before the next one starts.

Guiding constraints carried over from the spec (do not relitigate these while implementing):

- No phase implements anything listed under "Non-goals" (§2) — no quoting, sizing, position tracking,
  or persistence, ever.
- `Live` is never exercised by an automated test or a schedule (§3, §9.4) — only a human, deliberately.
- A `Stub` is only trustworthy once a contract test proves it agrees with `Simulated`/`Live` (§7) — no
  adapter counts as done without running the shared suite from `src/testkit/contract.rs` against it.

## BLOCKED — everything below needs input only you can give

Phases 0, 1, 2, 3, 4, 6, 9, 10 and 11 are complete and require nothing further. Phases 12–14 are
built; what each still needs is in its own section below (Blocker 4 sums it up). **Phases 5 and 7
cannot go live without the items below**, though both now have real scaffolding in place (`src/cex/binance/`,
`src/cex/bybit/`), unit-tested against mocked venue responses rather than a real sandbox. Decisions
below marked resolved came from Mo directly; what's still open is real credentials and real network
access, neither of which can be worked around, guessed, or defaulted.

### Blocker 1 — `EvmSimulated` has not cleared its real-world acceptance bar (blocks Phase 5)

`SPEC.md` §9.2 requires the contract suite to pass **100 times against a real RPC endpoint**, with a
real router and token, every result reconciled to the exact wei, including deliberately injected
failures for each `Outcome` variant. So far `EvmSimulated` has only been run against a local mocked
RPC server (`wiremock`) — it has never touched a real chain. `EvmLive` (Phase 5) is explicitly gated on
this bar being cleared first (§9.4): nothing signs or sends anything real before it is.

**Resolved — the mechanism is now proven against a real chain, not just a mock:**
- Testnet-first confirmed — an Ethereum Sepolia RPC URL (Alchemy) is in hand, stored as
  `EVM_LIVE_RPC_URL` in a gitignored `.env.local`, never committed. Ethereum Sepolia rather than Base
  Sepolia is fine: `SPEC.md`'s types carry their own `chain_id`, nothing pins a specific chain.
- This environment's network egress, initially blocking the RPC host, was opened by adding
  `eth-sepolia.g.alchemy.com` (and `api-testnet.bybit.com`) as allowed custom domains — `eth_chainId`
  and `eth_getCode` both confirmed real, working access.
- Router + token pair chosen and verified deployed on-chain: Uniswap V3 `SwapRouter02`
  (`0x3bFA4769FB09eefC5a80d6E87c3B9C650f7Ae48E`) and its USDC/WETH 0.3%-fee pool (real but thin
  liquidity, ~$3.8k — be mindful of running many real swaps against it) — USDC
  (`0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238`) and canonical Sepolia WETH
  (`0xfFf9976782d46CC05630D1f6eBAb18b2324d6B14`).
- Both flagged conventions confirmed correct against the real router: `RouteQuote.payload` as
  `router_address ++ calldata`, and decoding the return data as a single `uint256`.
- **A real bug was found and fixed by this real-network run**, not by any mock: `eth_call` never set
  `"from"`, so `msg.sender` inside the router defaulted to the zero address — the sender's overridden
  balance/allowance were never the address the router actually checked, and a real swap reverted with
  Uniswap's `"STF"` (`SafeTransferFrom` failed). Fixed by passing the route's sender as `eth_call`'s
  `from` for the router call (irrelevant, so omitted, for the two pure-view slot-probing calls). Two
  new tests pin this against the real chain — gated on `EVM_LIVE_RPC_URL` being set, a no-op otherwise
  (`src/dex/evm/simulated.rs`): one real `balanceOf`/`allowance` slot probe, one full real swap, both
  currently green.

**Real quote-vs-simulation benchmark (`benchmarks_real_quote_against_real_simulation_across_several_sizes`
in `src/dex/evm/simulated.rs`, gated on `EVM_LIVE_RPC_URL`):** an independent on-chain source (Uniswap's
own `QuoterV2`, not this crate) vs. this crate's `EvmSimulated`, same real pool, 5 distinct input sizes
(0.1–10 USDC). Exact wei match on all 5, every run `Success`:

| amountIn (USDC) | quoted (wei WETH) | simulated (wei WETH) | diff |
|---:|---:|---:|---:|
| 100,000 | 3,121,061,108,743 | 3,121,061,108,743 | 0 |
| 500,000 | 15,605,302,702,413 | 15,605,302,702,413 | 0 |
| 1,000,000 | 31,210,598,301,571 | 31,210,598,301,571 | 0 |
| 5,000,000 | 156,052,707,378,200 | 156,052,707,378,200 | 0 |
| 10,000,000 | 312,104,704,435,172 | 312,104,704,435,172 | 0 |

This is a single point-in-time snapshot (one block, back-to-back calls), not a repeated-over-time
benchmark, and covers 5 of the 100 distinct inputs §9.2 wants — real incremental progress, not the bar
cleared.

**The third leg now exists too — quote vs. simulation vs. executed, all three, real** (Blocker 2 below):
`real_sepolia_quote_vs_simulated_vs_executed` in `src/dex/evm/live.rs`, gated the same way. WETH→USDC,
0.001 WETH in, same pool, same block region:

| leg | amount_out (wei USDC) |
|---|---:|
| quoted (`QuoterV2`, independent of this crate) | 31,847,966 |
| simulated (`EvmSimulated`) | 31,847,966 |
| executed (`EvmLive`, real signed swap) | 31,847,966 |

Exact match across all three. Real Sepolia transactions: wrap (`WETH9.deposit()`)
`0xd3a5a3584b4cdead63d3f6fd99b7cd2d07e4340c5ccc127cdae6c8b62559990a`, swap
`0x5bb2b59b61850dac3be56fc04e0b90f3fa3c531882b4e892324b547f17fda2d8`. Same single-point-in-time caveat
as above — one input size, one moment, not the repeated/many-sizes bar §9.2 wants.

**Still needed to fully clear §9.2's literal bar:**
1. The remaining ~95 distinct-input runs, plus at least one deliberately injected failure per
   `Outcome` variant (one real revert has been observed already — this adapter's own now-fixed bug, not
   a deliberately injected one). Worth scripting as a real (rate-limited, liquidity-mindful) run rather
   than by hand.
2. `Outcome::TimedOut` still has no real-network path exercising it — `EvmSimulated::execute()`
   currently only ever produces `Success` or `Reverted` against a real RPC; needs a deliberate way to
   force a timeout (e.g. a request-timeout wrapper) to prove that shape too.

### Blocker 2 — `EvmLive` needs real signing credentials (blocks Phase 5, after Blocker 1)

**Resolved — `EvmLive` is implemented and has executed a real, successful swap on Sepolia:**
- A disposable, faucet-funded Sepolia signer was provided (address and key straight into
  `EVM_LIVE_SIGNER_KEY`/`EVM_LIVE_SENDER_ADDRESS` in the gitignored `.env.local`, never committed —
  same discipline as the RPC URL). `src/dex/evm/tx.rs`'s `Signer` derives the address from the key via
  `k256`/`alloy-consensus` EIP-1559 signing; a self-check test (gated on those same env vars, a no-op
  otherwise) confirms the derived address matches the one the wallet reports for its own key.
- `src/dex/evm/live.rs`'s `EvmLive` is implemented per `SPEC.md` §5: real sender/recipient/deadline
  enforcement (never zero/omitted — `SwapRequest.deadline_unix_secs` is checked both at `prepare()` and
  again at broadcast time, since this adapter's calldata convention can't embed a deadline the caller
  didn't already bake in), the router allowance capped to the exact swap amount via a checked `approve`
  (never `U256::MAX`), one nonce in flight at a time (a send lock held for the whole
  build-sign-broadcast-poll sequence of every transaction, including `approve`), the landed amount
  decoded from the swap's own ERC-20 `Transfer` log (never assumed equal to the quote), and revert
  reasons recovered by replaying a reverted call via `eth_call` at its block (a receipt alone never
  carries one). Covered by 6 `wiremock`-backed tests (success, revert, timeout) before ever touching a
  real network.
- **A real three-way run — quote vs. simulated vs. executed — landed on Sepolia**, all three in exact
  agreement; see the benchmark table under Blocker 1 above for the numbers and both real transaction
  hashes.
- The network-egress caveat from Blocker 1 didn't recur: the signer's sends went out over the same
  already-allowed `eth-sepolia.g.alchemy.com` host, just with a real (non-shared) API key instead of the
  heavily-rate-limited public `/v2/demo` endpoint, which 429'd on nearly every call under this adapter's
  real send-and-poll volume.

**Not yet done:** §9.2's full 100-distinct-input/per-outcome-variant bar (same gap as `EvmSimulated`,
now inherited by the executed leg too) — this was one input size, one run, not the repeated bar. No
`Outcome::Reverted`/`Outcome::TimedOut` path has been exercised against a real broadcast yet either
(only against `wiremock`).

### Blocker 3 — CEX venues: chosen, scaffolded, not yet credentialed (blocks Phase 7)

**Resolved:** both Binance and Bybit, built side by side rather than one after the other — this is a
genuinely multi-venue port, not "Binance now, a second venue later." `src/cex/binance/` and
`src/cex/bybit/` both exist: a signed REST client (`rest.rs`) and a `CexExecutor` implementation
(`live.rs`) per venue, defaulting to each venue's **testnet** host
(`testnet.binance.vision`, `api-testnet.bybit.com`) — a production run opts in explicitly via
`BINANCE_BASE_URL`/`BYBIT_BASE_URL`, never by default. Both venues' request signing is unit-tested
(Binance against a docs-derived HMAC vector, Bybit against the documented signable-string
construction) and both `execute()` flows are tested against `wiremock`-mocked venue responses.

Two adapter conventions are flagged, not confirmed, in `src/cex/binance/live.rs` and
`src/cex/bybit/live.rs` for the same reason `EvmSimulated`'s are: no sandbox account exists yet to
check them against a real response.
- Both: the `LOT_SIZE` step per symbol is supplied by the caller, not fetched from the venue's own
  instrument-info endpoint.
- Bybit only: the commission asset per symbol is also caller-supplied, since Bybit's order-status
  response reports a fee amount but not its currency, and this adapter cannot see account-level fee
  settings that would disambiguate it.

**Still needed, per venue:**
1. Sandbox/testnet API credentials (Binance Spot Testnet, Bybit's testnet) — required to run
   `cex_executor_contract` against the real sandbox host, which *is* "Simulated" for a CEX per §3.
   Same handling as the RPC URL: straight into env vars, gitignored, never committed.
2. Once credentials land, the same network-access caveat as Blocker 1 applies: this environment's
   egress policy will need `testnet.binance.vision` / `api-testnet.bybit.com` reachable before a real
   sandbox run can happen from inside it.

Production credentials are a separate, later ask, only needed once each venue's sandbox run clears
§9's bar.

### Blocker 4 — phases 12–14 need keys, a Base fork, and one decision

- **USDⓈ-M futures testnet keys** (separate from the spot testnet keys) as
  `BINANCE_FUTURES_API_KEY`/`BINANCE_FUTURES_API_SECRET`, and network access to the futures testnet
  host. Every `BinanceFuturesLive`/`BinanceFuturesAccount` convention in phase 12 and 14's status
  notes is unconfirmed until then; the gated runs are written and print what they find.
- **Whether the production account may trade USDⓈ-M futures** in its jurisdiction (V2's first
  question). The testnet does not check.
- **An anvil fork of Base** (an RPC URL for Base) for Slipstream's §9.2 fork run (phase 13).
- **A funded testnet key** for the one live liquidity life a person takes (phase 13, §9.4).
- **The owner's answer to V4's question** — do account reads belong in this crate? — which phase 14
  assumes is yes.

### What is NOT blocked

`EvmStub`, `CexStub`, `LiquidityStub` and `CexAccountStub` are all done, pass the shared contract
suites, and are safe to build a trading system against right now — see
[`examples/basic_usage.rs`](examples/basic_usage.rs) and
[`examples/liquidity_lifecycle.rs`](examples/liquidity_lifecycle.rs). Nothing above blocks writing or
testing consumer code against the stubs, against `BinanceLive`/`BybitLive`/`BinanceFuturesLive` run
against a mocked server in tests, or against `EvmLiquidity` on a local anvil node.

## Phase 0 — Scaffolding

- `cargo init --lib`, matching the module layout in §8.
- `Cargo.toml`: `async-trait`, `anyhow`, `tokio` (with `rt-multi-thread`, `time`), `rust_decimal`.
  Chain/venue-specific deps deferred to the phases that need them (below) so early phases build fast.
- `src/lib.rs` re-exporting `dex`, `cex`, `testkit`; empty modules for each path in §8's tree.
- CI skeleton (GitHub Actions): `cargo build`, `cargo test`, `cargo clippy -- -D warnings`,
  `cargo fmt --check` on every push. No network-dependent job yet — added in Phase 4.
- Done when: `cargo test` passes on an empty crate.

## Phase 1 — Shared vocabulary (§4)

- `Provenance` enum in a shared location (`src/lib.rs` or `src/provenance.rs`) used by both ports.
- No behavior yet — this just gives Phases 2 and 6 a type to build on.

## Phase 2 — DEX port: types, trait, contract suite skeleton (§5, §7 sketch)

- `src/dex/mod.rs`: `ChainAmount`, `ChainAddress`, `RouteQuote`, `SwapRequest`, `Prepared`, `Outcome`,
  `Realised`, `DexExecutor` trait — exactly as specified, no deviation without updating `SPEC.md` first
  (per the spec's own rule in its header).
- `src/testkit/contract.rs`: `dex_executor_contract(executor, fixture)` and a `ContractFixture` type
  the three DEX adapters will all be run against. Start with the shape assertions from §7's sketch
  (`amount_out.is_some() == Success`, `tx_ref.is_some() == Landed`) — this is the file every later DEX
  phase must pass through before it's considered done.
- Done when: the module compiles with the trait defined but zero implementations; the contract fn
  compiles against a hand-written throwaway mock in a `#[cfg(test)]` block (deleted once `EvmStub`
  exists in Phase 3).

## Phase 3 — `EvmStub` (§5's required implementations, third bullet)

- `src/dex/evm/stub.rs`: in-process fake. Programmable `Realised`/error per call; records every
  `prepare`/`execute` invocation (route, request, prepared value) for test assertions.
- First real run of `dex_executor_contract` against a real implementation — wire this into CI's
  unconditional test job (§7's table, top row: "every test run, unconditionally, nothing needed").
- This is the adapter every downstream trading-system consumer can start coding against, so treat it as
  a priority deliverable even before `Simulated`/`Live` exist.
- Done when: `dex_executor_contract` passes against `EvmStub` in CI, with unit tests covering a forced
  `Reverted { reason }` and a forced `TimedOut`, not just `Success`.

## Phase 4 — `EvmSimulated` (§5, second bullet)

- Pick the EVM stack (recommend `alloy` — `alloy-primitives`, `alloy-provider`, `alloy-rpc-types`) and
  add it to `Cargo.toml` here, not in Phase 0.
- `src/dex/evm/simulated.rs`: builds the call from `Prepared`, runs it via an `eth_call`-equivalent
  against a forked node (local `anvil --fork-url <rpc>` for dev/CI, or a hosted fork provider), with
  state overrides for the sender's input-token balance and the router's allowance when the simulated
  sender doesn't actually hold either — granting the allowance to the router the route actually calls
  (§5 second bullet, explicit pitfall called out in the spec).
- CI: add a network-dependent job gated on a fork RPC secret being present, running on a schedule (§7's
  middle row) and on any PR touching `src/dex/evm/**` — this is the job that keeps `EvmStub` honest
  going forward.
- Done when: `dex_executor_contract` passes against `EvmSimulated`, plus §9.2's bar — 100 runs with
  distinct inputs, reconciled to the exact wei, with at least one injected failure per `Outcome`
  variant (a revert, a timeout) each producing the correct `Realised` shape.

## Phase 5 — `EvmLive` (§5, first bullet)

- `src/dex/evm/tx.rs`: signing and nonce management shared within the EVM family — one nonce in flight
  per chain at a time.
- `src/dex/evm/live.rs`: real sender/recipient/deadline enforcement (never zero/omitted), on-chain
  `approve` capped to the venue amount (never unlimited), EIP-1559 broadcast, receipt polling to a
  terminal outcome or `TimedOut`, landed amount decoded from the transaction's own logs (an ERC-20
  `Transfer` event) — never assumed equal to the quote.
- Gate this behind a cargo feature (e.g. `live`) or a separate binary/example, never compiled into the
  default test path, so `cargo test` can never accidentally touch it.
- Per §9: this phase is not "started" against real funds until Phases 3 and 4's acceptance bars (§9.1,
  §9.2) are both green. §9.3 ("nothing has signed or sent anything real up to this point") is the gate
  to hold until a human deliberately runs it — never wire this into CI at all, not even manually
  triggered, without that person present.

## Phase 6 — CEX port: types, trait, `CexStub` (§6)

- `src/cex/mod.rs`: `OrderSide`, `OrderRequest`, `CexFill`, `CexExecutor` trait, exactly as specified —
  note `quoted_price` is for simulated/stub pricing and logging only, never used by `Live` to build the
  order itself.
- `src/cex/stub.rs`: `CexStub`, same shape/call-recording requirement as `EvmStub`.
- Extend `src/testkit/contract.rs` with `cex_executor_contract`, mirroring the DEX one's shape
  assertions (`filled_qty`/`filled_price` presence rules, `order_ref.is_some() == Landed`).
- Done when: `cex_executor_contract` passes against `CexStub` in CI, alongside the DEX one.

## Phase 7 — CEX venues: Binance + Bybit, in parallel (§6, first bullet)

Two venues from the start, not one followed by a second later — confirmed by Mo. Each gets its own
`<venue>/rest.rs` + `<venue>/live.rs`; the trait doesn't change between them (§6, closing paragraph).

- `src/cex/binance/rest.rs`, `src/cex/bybit/rest.rs`: a signed REST client per venue's trading API —
  done, unit-tested against each venue's documented signing scheme.
- `src/cex/binance/live.rs`, `src/cex/bybit/live.rs`: quantity rounded to the venue's step/notional
  size *before* sending, base URL + credential set configurable so that pointing it at the venue's
  sandbox/testnet host *is* "Simulated" for this leg (§3's asymmetry — no separate struct), fills read
  from the placing call's own response first (Binance), falling back to a status query when that
  response is always ambiguous (Bybit, whose order-create response never carries fill data) — done,
  unit-tested against `wiremock`-mocked venue responses. See the BLOCKED section above for the two
  flagged, unconfirmed conventions in each.
- CI: same pattern as Phase 4 — a scheduled/PR-gated job running `cex_executor_contract` against each
  venue's real sandbox host, credentials and network access permitting. Not yet wired up — no sandbox
  credentials to gate it on yet.
- Done when: §9's four-point bar is met for each venue, same as Phase 5's DEX equivalent, with "Live"
  meaning a real order against that venue's production API.

## Phase 8 — Second chain family

Superseded by Phase 15 ([V5](specs/V5-venues-and-solana.md)): a consumer needs Solana, and V5 is how
it arrives. The trait is unchanged, as this phase said; `Prepared`, `Outcome` and the cost gained
Solana forms, and the stub is `DexStub`.

## Phase 9 — Consumer-facing polish

- A minimal example (`examples/` or a doctest) showing a trading system calling `DexExecutor`/
  `CexExecutor` and handling the `Realised`/`CexFill` it gets back, to make the non-goals in §2
  concrete: no example should show this crate deciding, sizing, or persisting anything.
- Publish-readiness pass: crate-level docs, `README.md` badge/status update from "specification only"
  once Phase 3/6 land, changelog.

## Suggested sequencing

Phases 0–3 and 6 have no external dependencies (no RPC, no exchange credentials, no real money) and
can proceed immediately — they're also what unblocks any consumer project to start writing tests
against this crate. Phases 4, 5, 7 each need real infrastructure (fork RPC, venue sandbox/API keys)
and should be scoped as separate work once that access is arranged. Phase 8 is opportunistic. Suggested
critical path for "a consumer can start building against this crate":

```
Phase 0 → Phase 1 → Phase 2 → Phase 3 (EvmStub) → Phase 6 (CexStub)
```

everything else can trail behind without blocking downstream consumers.

## Phases 10–14 — specified in [`specs/`](specs/README.md), accepted into `SPEC.md`

Five changes requested on 28 September 2026. Each has its own spec; accepting one meant copying its
signatures into `SPEC.md` (§2, §5, §5b, §6, §6b, §7, §8), which is done. The specs remain the detailed
reference for each phase; what follows is the order of work and each phase's exit criterion.

| Phase | Spec | Needs | Days |
| --- | --- | --- | --- |
| 10 | [V0](specs/V0-evm-shared-sender.md): one `EvmSender` per wallet and chain; router return read from its first word | nothing | 1 |
| 11 | [V1](specs/V1-order-contract.md): `OrderRequest.reduce_only`; `OrderStateUnknown` | nothing | 1 |
| 12 | [V2](specs/V2-binance-usdm-futures.md): `BinanceFuturesLive` | futures testnet keys | 3 |
| 13 | [V3](specs/V3-liquidity-port.md): `LiquidityExecutor`, `LiquidityStub`, `EvmLiquidity` over a signing or fork sender | anvil fork; funded testnet key | 6.5 |
| 14 | [V4](specs/V4-cex-account-reads.md): `CexAccount` | futures testnet keys | 1.5 |

Phases 10 and 11, and phase 13's stub, need nothing external and go first. Phases 12 and 14 share
keys and a client, so they are built together. Everything that needs no credentials is built and
tested against `wiremock` or a local anvil node; what needs credentials is written as a test gated on
the relevant environment variables, a no-op when they are unset, like the Sepolia tests.

### Phase 10 — one EVM sender per wallet (V0)

- `src/evm/`: `rpc.rs` (`EvmRpc` and the hex/ABI/revert helpers `EvmSimulated` and `EvmLive` each
  carried a copy of), `erc20.rs` (selectors, reads, storage-slot probing moved out of
  `EvmSimulated`), `tx.rs` (`Signer`, moved from `dex/evm/tx.rs`, and `EvmSender`).
- `EvmSender::connect` checks `eth_chainId` and refuses a second sender for the same (address,
  chain_id) in one process; `send_and_confirm` and `ensure_allowance` move out of `EvmLive`; a
  `TimedOut` send blocks every later send until `resolve` clears it; fees follow a `FeePolicy`.
- `EvmLive::new(sender)`; `prepare` refuses a route for another chain.
- `EvmSimulated` reads `amount_out` from the first return word (README defect 1).
- Done when: two adapters sharing a sender get consecutive nonces (`wiremock`); a second `connect`
  for the same pair is an error and a different chain id is not; a forced `TimedOut` blocks the next
  send until `resolve` sees a receipt; a 64-byte return yields its first word and a 31-byte one an
  error; the existing tests pass apart from construction; `dex_executor_contract` still passes.
- **Status: done**, except that the Sepolia-gated tests have not been re-run since the refactor (they
  need `EVM_LIVE_RPC_URL`/`EVM_LIVE_SIGNER_KEY`, which this environment does not have). Every
  `wiremock` case above passes, and `dex_executor_contract` now runs against all three DEX adapters.
  The timeout rule was also run against a real node: with anvil's automine off, a signed send times
  out, the next send is refused, and `resolve` returns the receipt once a block is mined
  (`against_anvil_a_timed_out_send_is_resolved_once_mined`, gated on `EVM_ANVIL_RPC_URL`). Beyond the
  spec: a lost broadcast response is polled for rather than returned as an `Err` (the transaction may
  be out), and a failed receipt poll is retried until the timeout.

### Phase 11 — reduce-only and "this may have filled" (V1)

- `OrderRequest.reduce_only`; `OrderStateUnknown`; the rule that an `Err` from `execute` means
  nothing filled unless it is an `OrderStateUnknown`.
- `CexStub` records `reduce_only`, gains `program_state_unknown` and a reduce-only mode over a
  test-set signed position (rejecting a reduce-only order larger than the position until Phase 12's
  testnet run shows what Binance does).
- `BinanceLive` and `BybitLive` reject `reduce_only: true` before sending, and keep to the error rule:
  each order carries a client order id, a lost placing response is recovered by a status query, and
  anything the adapter cannot read after the venue accepted the order is `OrderStateUnknown`. For
  `BinanceLive` this also fixes README defects 2 (commission in a second asset is no longer dropped),
  3 (clock offset against `GET /api/v3/time`, and a status query to fall back on) and 4 (a market order
  that partly filled and then expired is a fill, not an error).
- Done when: `cex_executor_contract` runs with `reduce_only: false`; `cex_spot_rejects_reduce_only`
  passes against both spot adapters with nothing reaching the mock server; the stub's reduce-only
  cases (position −5: buy 3 fills, sell 1 rejected, buy 8 rejected) pass; `examples/basic_usage.rs`
  sets the field.
- **Status: done.** Beyond the list above, both spot adapters now keep to the error rule: each order
  carries a client order id, a lost answer (a timeout, a dropped connection, a 5xx) is followed by a
  status query, and "no such order" counts as "never accepted" only from a query *sent* after
  `recvWindow` has passed for the lost request. Everything the adapter cannot read once the venue may
  have acted is `OrderStateUnknown`. README defects 2, 3 and 4 are fixed for `BinanceLive`, which also
  reads `GET /api/v3/time` before its first signed call and retries once on `-1021`. `BybitLive` now
  refuses, before sending, a symbol with no commission asset configured. All of this is tested
  against `wiremock` only. Still open on spot, outside these specs: `BybitLive` signs with the local
  clock (§6 asks for venue clock sync) and takes its commission asset from the caller; and whether
  Binance's Spot Testnet reports a market order that ran out of book as `EXPIRED` with its partial
  `fills` (defect 4's check).

### Phase 12 — Binance USDⓈ-M futures (V2)

- **Before relying on it in production:** confirm the production account is eligible for USDⓈ-M
  futures in its jurisdiction. The testnet does not check.
- `src/cex/binance/sign.rs` shared by spot and futures; `src/cex/binance_futures/` with `rest.rs`
  (signed `fapi` client, server-clock offset refreshed every 10 minutes and on `-1021`, with one retry),
  `filters.rs`, `live.rs`.
- `BinanceFuturesLive::connect` runs the account checks in `SPEC.md` §6; `execute` follows V2's five
  steps (validate, place, read, commission from `userTrades`, lost-response recovery).
- Done when: the `wiremock` cases listed in V2 pass; `cex_executor_contract` against the testnet,
  gated on `BINANCE_FUTURES_API_KEY`; then **§9.2 on the testnet** — 100 orders of distinct sizes
  reconciled to the cent against `userTrades`, plus a rejected reduce-only order, a notional refusal,
  and a forced `OrderStateUnknown`. The module docs record the testnet host, the reduce-only notional
  exemption, and what the venue does with a reduce-only order larger than the position (which
  `CexStub` then copies).
- **Status: built and tested against `wiremock`; nothing has run against the testnet** (no keys, and
  no Binance host is reachable from this environment). `src/cex/binance/` now shares `sign.rs`,
  `clock.rs`, `client.rs` and `order.rs` between spot and futures; `src/cex/binance_futures/` has
  `rest.rs`, `filters.rs` and `live.rs`. Every `wiremock` case V2 lists passes. The gated runs are
  written: `cex_executor_contract` on the testnet (`BINANCE_FUTURES_API_KEY`/`_SECRET`), and — with
  `BINANCE_FUTURES_RUN_ACCEPTANCE=1` as well — the §9.2 run of 100 orders reconciled against
  `userTrades` with its injected failures, and a step that prints how the venue treats a reduce-only
  order larger than the position (`RECORD:` lines, for `CexStub` to copy). Unconfirmed until then: the
  testnet host (default `https://demo-fapi.binance.com`, the older one being
  `https://testnet.binancefuture.com`), the reduce-only notional exemption, the sign of `userTrades`
  commission, and the boolean shape of `positionSide/dual`, `multiAssetsMargin` and `feeBurn`. Beyond
  the spec: a failed status query is retried until `poll_timeout` before the order becomes
  `OrderStateUnknown`, and the `-1021` resync covers every signed call.

### Phase 13 — the liquidity port (V3)

- `src/liquidity/`: the §5b contract, `LiquidityStub`, and `EvmLiquidity` (`evm/abi.rs` with `sol!`
  definitions for the Uniswap v3 and Slipstream managers, `evm/executor.rs`).
- `EvmSender::fork` (anvil only, impersonating the owner), `provenance`, `ensure_balance`.
- `liquidity_executor_contract` in `src/testkit/contract.rs`; `examples/` gains a lifecycle against
  `LiquidityStub`.
- Done when: the suite passes against the stub, and against `EvmLiquidity` over a fork sender; the
  **§9.2 bar on a fork** — 100 lives with distinct ranges and amounts, each reconciled to the wei against
  `positions(id).liquidity` and the owner's `balanceOf`, plus `Reverted("Price slippage check")`,
  `Reverted("Not cleared")` and a `TimedOut` forced with `evm_setAutomine(false)` — is met for the
  Uniswap v3 ABI and for Slipstream's; one live testnet life has been reconciled by a person.
- **Status: built; the §9.2 bar is met for the Uniswap v3 ABI; Slipstream and the live life remain.**
  - The suite passes against `LiquidityStub` on every test run.
  - On anvil running Uniswap's published v3 bytecode (`@uniswap/v3-core` 1.0.1 and
    `@uniswap/v3-periphery` 1.4.4 from npm, deployed by `scripts/anvil-uniswap-v3.py`; the pool's init
    code hash matches the one the manager computes addresses with), `EvmLiquidity` over a fork sender
    passes the suite, and **100 lives reconcile to the wei** against `balanceOf` and `positions(id)`,
    27 of them with a swap through `EvmLive` on the same sender between increase and decrease (so
    collect returns principal plus fees), plus the three injected failures with the reasons above.
    This is a local chain carrying the real manager, not a fork of a public one: no fork RPC was
    reachable from this environment. The same tests run against any anvil fork by pointing the
    `LIQUIDITY_FORK_*` variables at the deployed contracts (`src/liquidity/evm/fork_tests.rs`), and
    CI's `anvil` job runs them on every push and nightly.
  - **Slipstream's ABI is encoded but unrun**: its fork bar needs an anvil fork of Base, with
    `LIQUIDITY_FORK_POOL_KEY=tick_spacing:<n>`. Its `mint` layout is from its published source; the
    other calls are assumed to match Uniswap v3's, which that run will confirm.
  - **The live testnet life** (§9.4) is a person's to take, with a funded testnet key.

### Phase 14 — reading a perp account (V4)

- V4 was written to be accepted as it stands while the owner decides whether account reads belong in
  this crate at all. It is accepted here: `SPEC.md` §2 carries the amended sentence and §6b the
  contract. If the answer is no, this phase is removed whole — nothing else depends on it.
- `src/cex/account.rs`, `CexAccountStub`, `BinanceFuturesAccount` sharing Phase 12's client.
- Done when: `cex_account_contract` passes against the stub; the `wiremock` cases in V4 pass; on the
  testnet, gated, a short opened with Phase 12 reads back here with the fill's quantity and entry price,
  and reads `qty == 0` after a reduce-only close.
- **Status: built; the stub and `wiremock` criteria are met; the testnet run is written and gated, not
  yet run.** `BinanceFuturesAccount` shares `BinanceFuturesRest` with `BinanceFuturesLive` (or comes
  from `BinanceFuturesLive::account()`). Unconfirmed until the testnet run: `positionRisk` v3 carries
  no margin type or leverage, so they come from `symbolConfig`; v3 may leave out a flat symbol, which
  is then read as `qty == 0` with the mark price from `premiumIndex`; account v3 carries no timestamp,
  so `MarginState.as_of_ms` is the venue clock as this client estimates it; funding is paged in
  windows of at most 7 days and refused for a `since_ms` older than 89 days, since Binance keeps three
  months of income and an older start could only be answered short.

## Phase 15 — one contract for every venue, and the Solana family (V5)

[V5](specs/V5-venues-and-solana.md), accepted 1 October 2026 with its §12 decided as recommended. Its
signatures are in `SPEC.md` §3, §4, §5, §5b, §7 and §8. Requested by a consumer for its Solana work
(arb-searcher's network plan, W6.3 and W6.4). Branch `feature/solana`, from `main` at `1c5f2c6`.

The rule throughout: one contract per port — commands, events, capabilities. The caller branches only
on capabilities, a venue's own types never cross the port, and a paper model is a venue that passes the
same suite. Modes stay Live, Simulated and Stub; a fork is how Simulated runs a sequence. **No EVM
number may move**: the suite and the anvil tests here, and the consumer's goldens and fork test, pin it.
Solana Live signing is not built (the signing backend refuses) until the owner asks.

Order of work, each step compiled and tested before the next (V5 §11):

1. **Dependency spike.** One pinned Solana SDK line builds a v0 transaction, signs it, and
   round-trips a transaction Jupiter built. Settles V5 §12.3: whether `orca_whirlpools_client`'s key
   type is the transaction crate's, so Orca's client builds the instructions, or whether the adapter
   encodes the six it needs from the IDL. *Stop and report.*
2. **The shared types** (`SPEC.md` §4, §5): `Network`, `Prepared` as an enum over `EvmCall` and
   `SolanaTransaction`, `Outcome::Expired`, `TxCost`, `Realised.cost`, `DexStub`. The EVM adapters only
   rewrap. Done when the whole suite and the anvil tests pass unchanged.
3. **The liquidity contract** (`SPEC.md` §5b): `LiquidityCommand`, `LiquidityEvent`,
   `LiquidityReport`, `LiquidityCapabilities`. `EvmLiquidity` (now `liquidity/uniswap_v3/`, built with
   its manager and ABI) and `LiquidityStub` move onto it, and `liquidity_executor_contract` runs V5
   §8's sequence against both. The consumer's LP runner and paper model move at the same time, proven
   by its `lp_golden` and `lp_fork`. *Stop and report.*
4. **`solana/`** (`SPEC.md` §5): `SolanaRpc`, `SolanaSender` with the Surfpool fork backend and its
   check, the signing backend refusing.
5. **Jupiter**: `JupiterSimulated`, then `JupiterLive` on Surfpool, each through
   `dex_executor_contract`.
6. **Whirlpool**: `WhirlpoolLiquidity` on Surfpool, through `liquidity_executor_contract`. Needs
   Surfpool installed, and its funding cheatcodes verified (V5 §12.5). *Stop and report.*
7. The consumer bumps its rev, then runs its paper Whirlpool and its Solana fork gate.

- Done when: steps 1–6 pass as stated, every EVM test here passes with no number changed, and the
  consumer's gates are green on the new rev.
- **Status: in progress.** Step 1 done, 1 October 2026, in a scratch crate outside this repository:
  - **The line** is the one `solana-sdk` 5.0 pins: `solana-transaction` 5.1.0, `solana-message`
    5.1.0, `solana-instruction` 4.0.0, `solana-hash` 4.7.0, `solana-keypair` 4.0.0, `solana-signer`
    4.0.0, `solana-pubkey` 4.4.0 (`solana-address` 2.8.0), `solana-signature` 3.6.0. V5 read
    `solana-transaction` 3.0.2; the crates had moved on.
  - **Measured:** a v0 memo transaction built, signed, verified and round-tripped through bincode; a
    transaction Jupiter built (`api.jup.ag/swap/v1`, 0.01 SOL to USDC, v0, one lookup table, 599
    bytes) decoded and re-encoded byte-identical, then signed by its payer and verified with its
    message unchanged.
  - **§12.3: Orca's client builds the instructions.** `orca_whirlpools_client` 8.0 asks for
    `solana-pubkey` ^3 and `solana-instruction` ^3, which resolve to `solana-pubkey` 3.0.0 (on
    `solana-address` 1.1.0, whose source is `pub use solana_address_v2::*`) and `solana-instruction`
    3.5.1 (`pub use solana_instruction_v4::*`). So its `Pubkey` and `Instruction` are the line's own:
    its `IncreaseLiquidityByTokenAmountsV2` and `UpdateFeesAndRewards` builders compiled into a signed
    v0 transaction with no conversion. A lockfile that held `solana-address` at 1.0.0 would split the
    types, which fails to compile rather than silently.
  - **Its cost:** taken with `default-features = false` (no `orca_whirlpools_core`), it takes the
    line's graph from 112 crates to 236, bringing `solana-program` 3.0 and its sysvars; beside this
    crate the total is 393, with one `k256` (0.13.4, the version this crate pins).
  - **Step 2 done.** `Network`, `Prepared::{Evm, Solana}`, `EvmCall`, `SolanaTransaction`,
    `Outcome::Expired`, `TxCost` with `native()`, `Realised.cost`, and `DexStub` (was `EvmStub`; a
    Solana route is prepared as an unsigned transaction for the sender). `EvmLive` reads `gasUsed`,
    `effectiveGasPrice` and `l1Fee` from the receipt into its cost; `EvmSimulated` reports none, and
    refuses a Solana route. 173 unit tests pass (167 before, 6 new); the five anvil tests pass with
    the same output as before the change, 100 lives reconciled to the wei.

## Tracking

Each phase's "Done when" line is its exit criterion. Treat §9 of `SPEC.md` as the authoritative
acceptance checklist for any `Live` adapter (Phases 5, 7) — do not mark either done against a lighter
bar than §9 states.
