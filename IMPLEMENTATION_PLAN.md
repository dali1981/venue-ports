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

Phases 0, 1, 2, 3, 4, 6, and 9 are complete and require nothing further. **Phases 5 and 7 cannot start
or continue without the items below.** Nothing here can be worked around, guessed, or defaulted —
each one is either a credential Claude cannot generate, or a decision the spec deliberately leaves to
whoever integrates a real venue.

### Blocker 1 — `EvmSimulated` has not cleared its real-world acceptance bar (blocks Phase 5)

`SPEC.md` §9.2 requires the contract suite to pass **100 times against a real RPC endpoint**, with a
real router and token, every result reconciled to the exact wei, including deliberately injected
failures for each `Outcome` variant. So far `EvmSimulated` has only been run against a local mocked
RPC server (`wiremock`) — it has never touched a real chain. `EvmLive` (Phase 5) is explicitly gated on
this bar being cleared first (§9.4): nothing signs or sends anything real before it is.

**Needed from you, all three:**
1. An RPC endpoint URL for an EVM chain (any public endpoint works — the state-override technique
   `EvmSimulated` uses needs no forked node and no paid tier).
2. A concrete DEX router address and a token pair on that chain to run the 100 test swaps against.
3. Confirmation of the two conventions this session picked in the absence of a real router to test
   against (documented at the top of `src/dex/evm/simulated.rs`, flagged but never confirmed):
   - `RouteQuote.payload` is read as `router_address (20 bytes) ++ calldata`.
   - The router's return data is decoded as a single `uint256` (`amount_out`) — will not work if the
     real router returns an array or tuple instead.

### Blocker 2 — `EvmLive` needs real signing credentials (blocks Phase 5, after Blocker 1)

Once Blocker 1 clears, `EvmLive` (`src/dex/evm/live.rs`, `src/dex/evm/tx.rs` — currently empty
placeholders) signs and broadcasts real transactions. This needs, from you, when you are ready to
spend real funds testing it:
- A private key or signer to hold the funds and sign with (never asked for or stored by Claude on its
  own initiative — this is a human-triggered action per §3/§9.4, always).
- The chain and the real router/tokens to trade, and the amount you're willing to risk on first runs.

### Blocker 3 — No CEX venue has been chosen or credentialed (blocks Phase 7)

`src/cex/<venue>/` does not exist yet because no venue has been picked. `SPEC.md`'s own example symbol
(`"SOLUSDT"`) suggests Binance, but this is a guess, not a confirmed decision — every venue has its own
signing scheme, rate limits, and symbol/step-size rules that are not swappable later without rework, so
this needs deciding up front, not defaulted.

**Needed from you, both:**
1. Which venue to build first (confirm Binance, or name a different one).
2. Sandbox/testnet API credentials for that venue — required before `CexLive` can be pointed at the
   sandbox host, which *is* "Simulated" for a CEX per §3 (no separate struct). Production credentials
   are a separate, later ask, only needed once the sandbox run clears §9's bar.

### What is NOT blocked

`EvmStub` and `CexStub` are both done, pass the shared contract suite, and are safe to build a trading
system against right now — see [`examples/basic_usage.rs`](examples/basic_usage.rs). Nothing above
blocks writing or testing consumer code against the stubs.

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

## Phase 7 — First CEX venue: `<venue>/rest.rs` + `CexLive` (§6, first bullet)

- Pick one venue (the spec's own example symbol format, `"SOLUSDT"`, suggests Binance — confirm before
  starting, since REST signing and rate-limit rules are venue-specific and not swappable later without
  rework).
- `src/cex/<venue>/rest.rs`: a signed REST client for that venue's trading API.
- `src/cex/<venue>/live.rs`: quantity rounded to the venue's step/notional size *before* sending, clock
  sync against the venue's server time where required, base URL + credential set configurable so that
  pointing it at the venue's sandbox host *is* "Simulated" for this leg (§3's asymmetry — no separate
  struct), fills read from the placing call's own response first, falling back to a status query only
  when that response was ambiguous.
- CI: same pattern as Phase 4 — a scheduled/PR-gated job running `cex_executor_contract` against the
  sandbox host, credentials permitting.
- Done when: §9's four-point bar is met for this venue, same as Phase 5's DEX equivalent, with "Live"
  meaning a real order against the venue's production API.

## Phase 8 — Second chain family (optional, lower priority)

- A Solana-style `Live`/`Simulated` pair implementing the same `DexExecutor` trait (§5, final
  paragraph) — `EvmStub` is reused as-is; the trait does not change.
- Defer until at least one consumer actually needs a second chain family — the spec explicitly treats
  this as an extension point, not a launch requirement.

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

## Tracking

Each phase's "Done when" line is its exit criterion. Treat §9 of `SPEC.md` as the authoritative
acceptance checklist for any `Live` adapter (Phases 5, 7) — do not mark either done against a lighter
bar than §9 states.
