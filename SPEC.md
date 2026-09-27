# Specification — `venue-ports`

Status: **specification only, nothing implemented yet.** This document is the target to implement
against. Where it gives a Rust signature, that signature is the contract — implement it as written
unless a limitation forces a change, in which case change this document first.

## 1. Why this crate exists

Every automated trading system that touches more than one venue ends up writing the same handful of
things, badly, more than once: how to sign and send a transaction and know when it landed, how to
place an order on an exchange and know when it filled, how to round a size to what the venue will
actually accept, and how to test any of that without spending real money or depending on a testnet
being up.

This crate is that layer, built once, so a new trading project starts from a connection that has
already been proven against real venues — instead of from zero.

## 2. Non-goals

This crate does **not**:

- fetch price quotes or discover routes (it executes a route/order it is *given*, it does not go and
  find one);
- decide whether a trade is worth taking, or size one;
- track positions, balances, or capital, or enforce any risk policy;
- persist anything — every call returns a value; what the caller does with it (log it, write it to a
  database, throw it away) is entirely the caller's concern;
- know anything about a specific trading strategy. A market-making system, a directional system, and
  an arbitrage system are all equally valid consumers, and none of their vocabulary belongs here.

Everything above is a trading system's job, built on top of this crate, never inside it.

## 3. The three-mode model

Every port in this crate (one per venue class — see §5, §6) has up to three implementations. Not all
three are optional if the goal is a connection that is both *proven* and *cheap to build against*:

| mode | needs | what it proves | cost |
|---|---|---|---|
| **Live** | real credentials, a real account, network access to the real venue | ground truth — what a trade actually costs and does | real money, per call |
| **Simulated** | a forked chain node (for a DEX) or a venue's own sandbox/testnet (for a CEX) | that the adapter is *wired correctly*: signing, encoding, and whatever validation the real venue applies, all without spending anything | setup per environment (fork infrastructure or sandbox credentials), network access, occasional flakiness |
| **Stub** | nothing | that code built *on top of* this crate — a strategy's retry logic, its halt conditions, its handling of a partial fill — behaves correctly under outcomes that are slow or impossible to force on demand against a real sandbox (a specific revert reason, a transaction that never confirms, a rejected order) | nothing to run, but only trustworthy under §7's rule |

None of the three is a substitute for either of the others:

- **Simulated** cannot be skipped in favour of **Stub** — nothing proves the real protocol accepts
  what was built until something real (even a sandbox) runs it.
- **Stub** cannot be skipped in favour of **Simulated** — a sandbox cannot deterministically produce
  every outcome a caller's logic needs to be tested against, and requiring sandbox credentials before
  a new project can write its first test against this crate defeats the point of it being reusable.
- **Live** is what actually trades, and is the only mode this document expects to be exercised by a
  human, deliberately, never by an automated test suite.

One asymmetry is worth stating up front: a CEX's own sandbox/testnet typically speaks the *same wire
protocol* as its production API — so "Simulated" for a CEX adapter is usually the **same
implementation as Live, pointed at a different host with different credentials**, not a separate code
path. A DEX's "run it and discard the result" (simulate-only) and "sign and broadcast" (live) are
*different operations* against the chain, so a DEX adapter genuinely needs three distinct
implementations where a CEX adapter typically needs two.

## 4. Shared vocabulary

```rust
/// Whether an outcome came from something that was actually sent, or from
/// something that was run and thrown away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// The call was run (an `eth_call`, a dry-run endpoint, a stub) and its
    /// result was never sent anywhere real.
    Simulated,
    /// A real transaction or order was actually sent to the venue.
    Landed,
}
```

Every outcome type below carries a `Provenance`. A caller that cannot tell, by looking at a value,
whether it came from a real send or a throwaway run has been handed a bug; the type exists so that
question always has an answer.

## 5. The DEX port

```rust
use async_trait::async_trait;
use anyhow::Result;

/// A chain-native token amount. This crate does not interpret decimals,
/// symbols, or USD value — that belongs to whatever quoted the route.
pub type ChainAmount = u128; // or a wrapper over the chain SDK's own 256-bit
                             // integer type, e.g. `alloy_primitives::U256`

/// An address on the chain in question. Left abstract here; the concrete
/// implementation for a given chain family uses that chain's own type
/// (e.g. `alloy_primitives::Address` for EVM chains, a base58 pubkey for
/// Solana-style chains).
pub type ChainAddress = Vec<u8>;

/// A route already priced by something upstream of this crate (an
/// aggregator's quote endpoint, a pool's own math, a router's simulation).
/// This crate turns it into a transaction and runs it — it never fetches
/// one itself.
#[derive(Debug, Clone)]
pub struct RouteQuote {
    pub chain_id: u64,
    pub token_in: ChainAddress,
    pub token_out: ChainAddress,
    pub amount_in: ChainAmount,
    pub expected_amount_out: ChainAmount,
    /// Opaque payload from whatever produced this quote — a route object,
    /// a serialized call, anything the adapter that consumes it knows how
    /// to turn into calldata. Never inspected outside the adapter.
    pub payload: Vec<u8>,
}

/// What the caller wants built on top of a `RouteQuote`.
#[derive(Debug, Clone)]
pub struct SwapRequest {
    pub sender: ChainAddress,
    pub recipient: ChainAddress,
    /// The minimum acceptable output, as a literal amount. Never a
    /// percentage or basis-point tolerance recomputed at build time — the
    /// caller has already decided the number that makes this worth doing,
    /// and that is the number a revert should be measured against.
    pub min_amount_out: ChainAmount,
    /// Unix timestamp after which the built transaction must not execute.
    /// A swap without one that lands late still lands — a caller that
    /// cares when it lands must set this.
    pub deadline_unix_secs: u64,
}

/// A route bound to a request: ready to run, one way or another. What
/// `to`/`calldata`/`value` actually mean is chain-specific; this shape is
/// deliberately generic across chain families.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub to: ChainAddress,
    pub calldata: Vec<u8>,
    pub value: ChainAmount,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Success,
    /// The swap reverted. There is no output amount; there is a reason.
    Reverted { reason: String },
    /// A transaction was sent but never reached a terminal state inside
    /// the adapter's own timeout. This is not a revert: the money's state
    /// is genuinely unknown and must be resolved out of band (a receipt
    /// poll that outlives this call, a manual check) before it is treated
    /// as anything else.
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct Realised {
    /// `None` exactly when `outcome` is not `Success`. A revert or a
    /// timeout must never be represented as a zero amount — a zero is a
    /// real, terrible price; "no price" is a different fact.
    pub amount_out: Option<ChainAmount>,
    pub outcome: Outcome,
    /// The block (or slot) the outcome was observed at.
    pub at: u64,
    pub provenance: Provenance,
    /// Set if and only if `provenance == Provenance::Landed`.
    pub tx_ref: Option<Vec<u8>>, // a transaction hash / signature, chain-specific encoding
}

#[async_trait]
pub trait DexExecutor: Send + Sync {
    /// Turn a quoted route into something the network can run. May call
    /// out to whatever routing/build endpoint the venue exposes (e.g. an
    /// aggregator's "build this route into calldata" call) — this is
    /// still preparation, not execution, and nothing is sent yet.
    async fn prepare(&self, route: &RouteQuote, req: &SwapRequest) -> Result<Prepared>;

    /// Run the prepared swap to a terminal outcome. **Does not return
    /// until the outcome is known** — a simulated adapter returns as soon
    /// as its throwaway call answers; a live adapter signs, broadcasts,
    /// and polls until the transaction lands, reverts, or the adapter's
    /// own timeout elapses (`Outcome::TimedOut`). A caller must never have
    /// to separately "check what happened" after calling this.
    ///
    /// `at`, when given, asks the adapter to read state as of a specific
    /// block/slot instead of the latest one — used to re-run the same
    /// prepared call later, to see how far the answer has moved.
    async fn execute(&self, prepared: &Prepared, at: Option<u64>) -> Result<Realised>;

    /// A short, stable label for logging — e.g. `"evm-live"`,
    /// `"evm-simulated"`, `"evm-stub"`.
    fn label(&self) -> &'static str;
}
```

### Required implementations (DEX)

- **`EvmLive`** — signs and broadcasts a real EIP-1559 transaction, then polls for the receipt.
  Responsibilities beyond signing itself: a real sender/recipient and deadline on the built
  transaction (never the zero address, never omitted); the venue cap enforced as an on-chain
  `approve`, never an unlimited allowance; one nonce in flight per chain at a time; decoding the
  landed amount from the transaction's own logs (e.g. an ERC-20 `Transfer` event), never assumed equal
  to what was quoted.
- **`EvmSimulated`** — runs the prepared call against current (or a specified historical) chain state
  through a read-only simulation endpoint (an `eth_call`-equivalent), with no transaction ever
  broadcast. Where the simulated sender does not actually hold the input token or the router's
  allowance, the adapter is responsible for overriding just enough state (balance, allowance) to make
  the call possible — and for granting that allowance *to the router the route actually calls*, not to
  whatever address happens to be probing it.
- **`EvmStub`** — an in-process fake with no network calls at all. Must let a test program the exact
  `Realised` (or error) a given `execute()` call returns, including reverts with a specific reason and
  a forced `TimedOut`. Must record every call it received (route, request, prepared value) so a test
  can assert on what was actually sent to it, not just on what it returned.

A second chain family (e.g. a Solana-style chain, where transactions are signed and simulated
differently from an EVM chain) gets its own `Live`/`Simulated` pair implementing the same trait; the
trait does not change to accommodate it. `Stub` is chain-family-agnostic and does not need to be
duplicated per chain.

## 6. The CEX port

```rust
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct OrderRequest {
    /// The venue's own symbol spelling (e.g. `"SOLUSDT"`) — this crate does
    /// not maintain a symbol mapping.
    pub symbol: String,
    pub side: OrderSide,
    pub quantity: Decimal,
    /// The price this order was decided against. Used by a simulated or
    /// stub implementation to price the resulting fill; a live
    /// implementation may log it for comparison but must never use it to
    /// build the order itself — a market order carries no price, and
    /// treating this as one would be a fabricated quote, not a real order.
    pub quoted_price: Decimal,
}

#[derive(Debug, Clone)]
pub struct CexFill {
    pub filled_qty: Decimal,
    pub filled_price: Decimal,
    pub commission: Decimal,
    pub commission_asset: String,
    pub provenance: Provenance,
    /// Set if and only if `provenance == Provenance::Landed`.
    pub order_ref: Option<u64>,
}

#[async_trait]
pub trait CexExecutor: Send + Sync {
    /// Places (or simulates) the order and returns only once its outcome
    /// is known. Many venues answer a market order synchronously in the
    /// same call that placed it — read that response correctly rather
    /// than polling unnecessarily; fall back to a status query only when
    /// the placing call's own response is ambiguous (e.g. the connection
    /// was lost after the order was sent but before the response arrived).
    async fn execute(&self, req: &OrderRequest) -> Result<CexFill>;

    fn label(&self) -> &'static str;
}
```

### Required implementations (CEX)

- **`CexLive`** — places a real order against a venue's trading API. Responsible for: rounding
  `quantity` to whatever step/notional size the venue's own rules require *before* sending (an
  unrounded quantity is either rejected outright or, worse, silently accepted and left to drift from
  whatever a paired trade on another venue used); clock synchronisation against the venue's server
  time where the venue requires a timestamp within a tolerance; reading the fill correctly (§ above).
  Configurable by a base URL and a credential set, so that pointing it at a venue's sandbox/testnet
  host with sandbox credentials *is* the "Simulated" mode for this leg (§3) — no separate struct.
- **`CexStub`** — an in-process fake, same shape and same call-recording requirement as `EvmStub`:
  programmable fills, rejections, and partial fills, no network.

A second venue (a different exchange, with a different wire protocol) gets its own `Live`
implementation; the trait does not change.

## 7. Contract tests — what keeps the stub honest

A stub that quietly drifts from what `Simulated`/`Live` actually do is worse than no stub: code built
on top of it can pass every test and still be wrong the first time it meets a real venue. This is a
well-known failure mode for mocked dependencies in general, not specific to trading systems — a mock
that is never checked against the real thing eventually stops resembling it, and nothing notices until
production does.

The rule that prevents it: **one shared test suite per port, written generically over "any
`impl DexExecutor`" / "any `impl CexExecutor`"**, that every implementation must pass:

```rust
// sketch — the real suite lives in `src/testkit/contract.rs`
pub async fn dex_executor_contract(executor: &dyn DexExecutor, fixture: ContractFixture) {
    let prepared = executor.prepare(&fixture.route, &fixture.request).await.unwrap();
    let realised = executor.execute(&prepared, None).await.unwrap();

    // Shape assertions every implementation must satisfy, regardless of
    // mode:
    assert_eq!(realised.amount_out.is_some(), matches!(realised.outcome, Outcome::Success));
    assert_eq!(realised.tx_ref.is_some(), realised.provenance == Provenance::Landed);
}
```

Run three ways, at different points in the development cycle:

| run against | when | needs |
|---|---|---|
| `Stub` | every test run, unconditionally | nothing |
| `Simulated` (a fork / a sandbox) | before any change to a `Live` adapter ships, and on a recurring schedule even without a code change | fork infrastructure or sandbox credentials |
| `Live` | never automated — only a deliberate, watched, human-triggered run | real credentials, real money |

The middle row is what makes the top row trustworthy: if `Stub` and `Simulated` ever disagree on the
contract, the schedule catches it before anything is built on top of a fake that stopped matching
reality.

## 8. Module layout

```text
venue-ports/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── dex/
    │   ├── mod.rs           # DexExecutor, RouteQuote, SwapRequest, Prepared,
    │   │                     # Realised, Outcome (Provenance is shared, § 4)
    │   └── evm/
    │       ├── live.rs       # EvmLive
    │       ├── simulated.rs  # EvmSimulated
    │       ├── stub.rs       # EvmStub
    │       └── tx.rs         # signing + nonce management shared within the family
    ├── cex/
    │   ├── mod.rs            # CexExecutor, OrderRequest, CexFill
    │   ├── stub.rs           # CexStub
    │   └── <venue>/
    │       ├── rest.rs        # a signed REST client for one venue's API
    │       └── live.rs        # CexLive for that venue (base_url selects
    │                           # production vs. sandbox)
    └── testkit/
        └── contract.rs        # § 7's shared suite
```

## 9. Acceptance criteria

Before a `Live` implementation is considered done:

1. The contract suite (§7) passes against its `Stub` counterpart.
2. The contract suite passes against its `Simulated` counterpart at least 100 times with distinct
   inputs, every outcome reconciled to the unit (the exact wei / the exact cent), with at least one
   deliberately injected failure per outcome variant (a revert, a rejection, a forced timeout) each
   ending in the correct `Outcome`/`Realised` shape.
3. Nothing in the crate has signed or sent anything real up to this point.
4. Only after 1–3 pass does anything exercise `Live`, and only ever as a small, deliberate, watched
   action taken by a person — never from an automated test, never from a schedule.

## 10. What a consumer builds on top

Everything a trading system needs beyond this crate — deciding whether a trade is worth taking, what
size, what happens while a chain leg has landed but a paired order hasn't, how a paper/backtest mode
prices a fill differently from `Simulated` (e.g. modelling queue position, or partial fills against a
recorded book) — is that system's own logic, built by calling `DexExecutor`/`CexExecutor` and doing
something with the `Realised`/`CexFill` it gets back. None of it belongs in this crate, and a consumer
should not need to fork this crate to get it.
