# Specification — `venue-ports`

Status: the contract the crate implements. What is built so far, and to which bar, is tracked in
`IMPLEMENTATION_PLAN.md`. This document is the target to implement against. Where it gives a Rust signature, that signature is the contract — implement it as written
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
- track positions, balances, or capital — hold, compute, or cache them — or enforce any risk policy.
  *Reading* what a venue reports about an account, at the moment it is asked, is not tracking:
  nothing is kept between calls and nothing is derived (§6b);
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
  to what was quoted. It sends through a shared `EvmSender` (below) and never owns a key itself:

  ```rust
  impl EvmLive {
      pub fn new(sender: Arc<EvmSender>) -> Self;
  }
  ```

  `prepare` refuses a `RouteQuote` whose `chain_id` is not the sender's. Given a fork sender (§5b)
  instead of a signing one, `EvmLive` becomes a swap simulator whose state persists between calls: its
  provenance is then `Simulated` and it sets no `tx_ref`.
- **`EvmSimulated`** — runs the prepared call against current (or a specified historical) chain state
  through a read-only simulation endpoint (an `eth_call`-equivalent), with no transaction ever
  broadcast. Where the simulated sender does not actually hold the input token or the router's
  allowance, the adapter is responsible for overriding just enough state (balance, allowance) to make
  the call possible — and for granting that allowance *to the router the route actually calls*, not to
  whatever address happens to be probing it. `amount_out` is decoded from the **first 32-byte word**
  of the router's return data: `SwapRouter02.exactInputSingle` returns one word, and KyberSwap's
  `MetaAggregationRouterV2.swap` and the other common aggregator routers put the output amount first.
  Fewer than 32 bytes is an error, never a panic. A router whose output is not its first word needs its
  own adapter.
- **`EvmStub`** — an in-process fake with no network calls at all. Must let a test program the exact
  `Realised` (or error) a given `execute()` call returns, including reverts with a specific reason and
  a forced `TimedOut`. Must record every call it received (route, request, prepared value) so a test
  can assert on what was actually sent to it, not just on what it returned.

A second chain family (e.g. a Solana-style chain, where transactions are signed and simulated
differently from an EVM chain) gets its own `Live`/`Simulated` pair implementing the same trait; the
trait does not change to accommodate it. `Stub` is chain-family-agnostic and does not need to be
duplicated per chain.

### The EVM sender — one per wallet and chain

Every adapter that sends from an EVM wallet (`EvmLive`, and `EvmLiquidity` in §5b) sends through the
same `Arc<EvmSender>`. It is the only thing in the crate that assigns nonces, so two adapters can
never race for one: "one nonce in flight per chain at a time" holds for the wallet, not just for each
adapter.

```rust
// src/evm/tx.rs
/// The only thing in the crate that assigns nonces for one wallet on one
/// chain. Every adapter that sends from that wallet holds the same `Arc`.
pub struct EvmSender { /* EvmRpc, backend (Signer or fork owner), chain_id, FeePolicy,
                          poll settings, unresolved: Option<B256> */ }

pub enum TxOutcome {
    Success { block: u64, tx_hash: B256, logs: Vec<RpcLog> },
    Reverted { block: u64, tx_hash: B256, reason: String },
    TimedOut { tx_hash: B256 },
}

/// How max fee and priority fee are chosen: `base_fee_multiplier` × base
/// fee + priority fee, and `fallback_priority_fee_wei` when the node has no
/// `eth_maxPriorityFeePerGas`. The default (2×, 1.5 gwei) is tuned for
/// Sepolia; it is a per-chain setting.
pub struct FeePolicy { pub base_fee_multiplier: u32, pub fallback_priority_fee_wei: u128 }

impl EvmSender {
    /// Checks `eth_chainId` against `chain_id`. Returns an error if this
    /// process already holds an `EvmSender` for the same (address, chain_id).
    pub async fn connect(rpc: EvmRpc, signer: Signer, chain_id: u64, fees: FeePolicy) -> Result<Arc<Self>>;
    pub fn address(&self) -> Address;
    pub fn chain_id(&self) -> u64;

    /// Build, sign, broadcast and poll one transaction to a terminal
    /// outcome, holding the send lock throughout. Returns an error without
    /// sending if an earlier send timed out and has not been resolved.
    pub async fn send_and_confirm(&self, to: Address, calldata: Vec<u8>, value: U256) -> Result<TxOutcome>;

    /// If the sender's allowance to `spender` is below `amount`, approve
    /// exactly `amount`. Never `U256::MAX`. A revert or timeout here is an
    /// `Err`, a setup failure, and not the caller's outcome.
    pub async fn ensure_allowance(&self, token: Address, spender: Address, amount: U256) -> Result<()>;

    /// The hash of a send that ended `TimedOut` and is not yet resolved.
    pub fn unresolved(&self) -> Option<B256>;

    /// Polls the unresolved hash once. Returns its terminal outcome, which
    /// clears it. Returns `None` if it is still pending. Once the node no
    /// longer knows the hash and the account's `latest` nonce has moved past
    /// it, it was replaced or dropped: that is reported as an error naming the
    /// hash, and the hash is cleared.
    pub async fn resolve(&self) -> Result<Option<TxOutcome>>;

    /// The node this sender talks to, for reads alongside its sends.
    pub fn rpc(&self) -> &EvmRpc;
    /// How often, and for how long, a receipt is polled for before a send
    /// ends `TimedOut` (default: every 4 s, for 3 minutes).
    pub fn set_poll_settings(&self, poll: PollSettings);
}
```

A send whose broadcast response is lost may still have reached the node, so it is polled for like any
other and ends `TimedOut` (and unresolved) if no receipt appears; only a broadcast the node answered and
refused is an `Err`. A failed receipt poll is retried until the timeout, never returned as an `Err`
after the transaction is out.

Rules:

- **One `EvmSender` per (address, chain_id) per process.** `connect` enforces this with a process-wide
  registry, and the entry is removed on drop. Nothing can enforce it across processes: run one process
  per wallet per chain.
- **An `EvmSender` belongs to one chain.** Adapters refuse actions for any other `chain_id`.
- **Nothing is sent over an unresolved timeout.** After a `TimedOut`, every send is refused until
  `resolve` has cleared the hash. This makes `Outcome::TimedOut`'s "resolve before anything else" rule
  impossible to skip.
- **No convenience constructor builds a sender inside an adapter.** It would make a second sender for
  the same wallet the easy thing to write.

The EVM chain family's plumbing is shared by every EVM adapter rather than copied into each:
`src/evm/rpc.rs` (`EvmRpc`: JSON-RPC, `eth_call` with `from` and state overrides, receipts,
revert-reason replay, hex/ABI helpers) and `src/evm/erc20.rs` (selectors, balance and allowance reads,
storage-slot probing).

## 5b. The liquidity port

A concentrated-liquidity position goes through a **position manager**, not a router. It is created,
grown, shrunk, harvested and closed. `DexExecutor` cannot express this: `Realised` holds one
`amount_out`, while a mint returns a position id, an amount of liquidity and two token amounts, and a
decrease moves nothing until a later collect. The port turns a decided action into a known outcome like
the other two; it has its own trait because its outcomes have a different shape.

In this port's terms, the crate does **not**: choose ranges, or compute liquidity from prices or
prices from ticks (the manager computes the liquidity, the chain reports it, and the crate reads the
report back); value a position, track what it holds, or remember what it minted; stake positions in
gauges or claim emissions; create pools, or handle native ETH (`value` is always zero, and tokens are
ERC-20s). It does not support fee-on-transfer tokens: the EVM adapter detects them and refuses to read
their outcome, but does not handle them.

```rust
// src/liquidity/mod.rs
use crate::dex::{ChainAddress, ChainAmount, Outcome, Prepared};
use crate::Provenance;

/// Identifies the pool a range belongs to. It is also what decides how the
/// manager's `mint` is encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolKey {
    /// Uniswap v3's NonfungiblePositionManager and its ABI-identical forks
    /// (PancakeSwap v3): the fee tier in hundredths of a basis point
    /// (500 = 0.05 %).
    Fee(u32),
    /// Aerodrome and Velodrome Slipstream: the pool's tick spacing. Their
    /// `mint` takes one more argument, `sqrtPriceX96`, which this crate
    /// always sends as zero because it never creates a pool.
    TickSpacing(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeSpec {
    pub chain_id: u64,
    /// The position manager contract, not the pool.
    pub manager: ChainAddress,
    /// In the pool's own order: token0 < token1.
    pub token0: ChainAddress,
    pub token1: ChainAddress,
    pub pool_key: PoolKey,
    pub tick_lower: i32,
    pub tick_upper: i32,
}

/// A position held in a manager. On v3-style managers it is an ERC-721
/// token id, as 32 big-endian bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionRef {
    pub chain_id: u64,
    pub manager: ChainAddress,
    pub id: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum LiquidityAction {
    Mint {
        range: RangeSpec,
        amount0_desired: ChainAmount,
        amount1_desired: ChainAmount,
        amount0_min: ChainAmount,
        amount1_min: ChainAmount,
    },
    Increase {
        position: PositionRef,
        amount0_desired: ChainAmount,
        amount1_desired: ChainAmount,
        amount0_min: ChainAmount,
        amount1_min: ChainAmount,
    },
    /// Moves `liquidity` out of the range and into the position's owed
    /// tokens. **It transfers nothing**: tokens leave only on `Collect`.
    Decrease {
        position: PositionRef,
        liquidity: u128,
        amount0_min: ChainAmount,
        amount1_min: ChainAmount,
    },
    /// Transfers the owed tokens to the owner, up to the caps. Owed tokens
    /// are the principal released by earlier decreases plus the fees earned.
    Collect { position: PositionRef, amount0_max: u128, amount1_max: u128 },
    /// Destroys a position that has no liquidity and nothing owed.
    Burn { position: PositionRef },
}

#[derive(Debug, Clone)]
pub struct LiquidityRequest {
    /// Signs and pays. It also receives the minted position and the collected
    /// tokens: the crate never sends either anywhere else. Never the zero
    /// address.
    pub owner: ChainAddress,
    /// Unix time after which the action must not execute. Required, as for a
    /// swap.
    pub deadline_unix_secs: u64,
}

#[derive(Debug, Clone)]
pub struct LiquidityRealised {
    pub outcome: Outcome,
    /// The position acted on; for `Mint`, the new one.
    pub position: Option<PositionRef>,
    pub liquidity_delta: Option<u128>,
    pub amount0: Option<ChainAmount>,
    pub amount1: Option<ChainAmount>,
    /// The block the outcome was observed at.
    pub at: u64,
    pub provenance: Provenance,
    /// Set if and only if `provenance == Provenance::Landed`.
    pub tx_ref: Option<Vec<u8>>,
}

/// Returned inside `anyhow::Error` when an action's transaction landed but
/// its outcome could not be read from the receipt: an expected event was
/// missing, or the cross-check failed. Something happened on chain, so the
/// caller must inspect `tx_ref` before it acts on this position again.
#[derive(Debug, Clone)]
pub struct LandedUnread {
    pub tx_ref: Vec<u8>,
    pub reason: String,
}
impl std::error::Error for LandedUnread {}

#[async_trait]
pub trait LiquidityExecutor: Send + Sync {
    /// Validates the action and encodes it. It may read the chain (the
    /// position's owner and tokens) but sends nothing.
    async fn prepare(&self, action: &LiquidityAction, req: &LiquidityRequest) -> Result<Prepared>;
    /// Runs the prepared action to a terminal outcome and does not return
    /// before then, like `DexExecutor::execute`.
    async fn execute(&self, prepared: &Prepared) -> Result<LiquidityRealised>;
    fn label(&self) -> &'static str;
}
```

**The shape rules**, asserted for every implementation by `liquidity_executor_contract` (§7):

- `position`, `liquidity_delta`, `amount0` and `amount1` are all `Some` exactly when `outcome` is
  `Success`. A revert or a timeout is never shown as zeros.
- `tx_ref` is `Some` exactly when `provenance` is `Landed`.
- An `Err` means nothing was sent for the action itself, unless it is `LandedUnread`. A failed
  approval is an `Err`: it is setup, and no position changed.

**What the amounts mean, per action:**

| Action | `liquidity_delta` | `amount0`, `amount1` | Read from the manager's events |
| --- | --- | --- | --- |
| `Mint` | liquidity added | tokens paid in | ERC-721 `Transfer(0 → owner, id)` and `IncreaseLiquidity(id, …)` |
| `Increase` | liquidity added | tokens paid in | `IncreaseLiquidity(id, …)` |
| `Decrease` | liquidity removed | tokens **credited to the position's owed balance**, not transferred | `DecreaseLiquidity(id, …)` |
| `Collect` | 0 | tokens transferred to the owner | `Collect(id, …)` |
| `Burn` | 0 | 0, 0 | ERC-721 `Transfer(owner → 0, id)` |

The zeros for `Collect` and `Burn` are real values: those actions change no liquidity, and `Burn`
moves no tokens.

**There is no `at` parameter on `execute`.** A liquidity action changes state that later actions
depend on, so re-running one alone at an older block means nothing. A caller who wants an older state
starts the fork at that block.

### Required implementations (liquidity)

A position's life is a sequence of actions, each depending on the state the last one left, and an
`eth_call` discards that state. So the Simulated mode runs the actions on an **anvil fork**, as real
transactions from an impersonated owner: the same operation as Live, sent to a chain that is thrown
away afterwards. The result is one implementation and two senders — §3's CEX pattern, arrived at by a
different route — and Simulated and Live share every line of encoding and decoding.

- **`LiquidityStub`** — no network calls. Each call's outcome is programmed: success with given
  values, `Reverted { reason }`, `TimedOut`, an `Err`, or `LandedUnread`. It records every `prepare` and
  `execute` it receives. **An action with no programmed outcome is an `Err` naming the action**: a
  fallback would have to invent liquidity and amounts.
- **`EvmLiquidity`** — one implementation over an `Arc<EvmSender>`; its label is
  `"evm-liquidity-live"` or `"evm-liquidity-fork"`, from the sender.

  ```rust
  impl EvmLiquidity {
      pub fn new(sender: Arc<EvmSender>) -> Self;
  }
  ```

  `prepare` checks the owner is the sender's address and not zero, the deadline is set and has not
  passed, and the action's `chain_id` is the sender's. `Mint`: token0 < token1, tick_lower <
  tick_upper, the desired amounts are not both zero, and each minimum is at most its desired amount
  (tick spacing is the manager's to check). Any other action: `ownerOf(id)` must be the owner;
  `Increase` reads token0 and token1 from `positions(id)`; `Decrease` needs liquidity above zero;
  `Collect` needs at least one cap above zero. Encoding uses `alloy-sol-types`, never hand-encoding.

  `execute`: refuses a passed deadline without sending; for `Mint` and `Increase`, `ensure_balance`
  and `ensure_allowance` per token, with the manager as spender and **exactly the desired amount**,
  never `U256::MAX`; sends to the manager; decodes the events from the manager's own logs
  (`LandedUnread` if one is missing); **cross-checks** against the ERC-20 `Transfer` logs of the same
  receipt — for `Mint` and `Increase` the owner sent exactly `amount0`/`amount1`, for `Collect` it
  received exactly them, zero amounts skipped — and gives `LandedUnread` naming both figures on a
  mismatch; an amount above `u128` is `LandedUnread`, never truncated. `Reverted` and `TimedOut` map
  onto `Outcome` as they do for swaps.

  | Manager | `PoolKey` | ABI |
  | --- | --- | --- |
  | Uniswap v3 `NonfungiblePositionManager` | `Fee` | reference |
  | PancakeSwap v3 `NonfungiblePositionManager` | `Fee` | identical to Uniswap v3 |
  | Aerodrome / Velodrome Slipstream `NonfungiblePositionManager` | `TickSpacing` | `mint` takes `tickSpacing` in place of `fee` and ends with `sqrtPriceX96` (always 0) |

  Manager addresses are not listed in the crate: the consumer passes them in. Uniswap v4's
  `PositionManager` (`modifyLiquidities` with Permit2) needs a `PoolKey::V4` variant and a different
  event set, and is deferred until a consumer needs it.

The sender gains a fork backend for this mode:

```rust
impl EvmSender {
    /// A sender that impersonates `owner` on an anvil fork. It refuses any
    /// node whose `web3_clientVersion` does not start with "anvil": this
    /// backend sends transactions, and must never send them anywhere real.
    pub async fn fork(rpc: EvmRpc, owner: Address, chain_id: u64) -> Result<Arc<Self>>;

    /// `Simulated` for a fork sender, `Landed` for a signing one. Adapters
    /// take their provenance from this, and set `tx_ref` only when it is
    /// `Landed`.
    pub fn provenance(&self) -> Provenance;

    /// Checks that the owner holds at least `amount` of `token`. A signing
    /// sender returns an error when it does not, so a transaction that would
    /// revert with STF is never sent. A fork sender instead writes the balance
    /// slot with `anvil_setStorageAt` (the slot comes from `evm::erc20`'s
    /// probing), exactly as `EvmSimulated` overrides state.
    pub async fn ensure_balance(&self, token: Address, amount: U256) -> Result<()>;
}
```

On a fork, `send_and_confirm` calls `anvil_impersonateAccount` and then `eth_sendTransaction` from the
owner, and follows the same receipt polling, revert-reason replay, timeout path and unresolved-timeout
rule as signing. One difference is deliberate: when the node's gas estimate says a transaction will
revert, a signing sender refuses to spend gas on it (an `Err`, nothing sent), while a fork sender
sends it anyway with a fixed gas limit, so a fork run observes the revert — `"Price slippage check"`,
`"Not cleared"` — as an `Outcome::Reverted` with its reason. A fork sender is not in the process-wide
registry: a fork is its own node, and anvil assigns an impersonated account's nonces itself.

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
    /// The venue must refuse any part of this order that would increase or
    /// flip the position instead of reducing it. A venue with no positions
    /// (spot) must reject `true` with an error before sending, never ignore
    /// it: an ignored reduce-only is an unguarded order.
    pub reduce_only: bool,
}

/// Returned (inside `anyhow::Error`, found with `downcast_ref`) when an
/// order may have reached the venue but its outcome could not be read: the
/// placing call's response was lost, and the status query that should
/// follow it also failed. The venue may have filled some, all or none of
/// it. The caller must find out before it acts on this symbol again, for
/// example by reading its position.
#[derive(Debug, Clone)]
pub struct OrderStateUnknown {
    pub symbol: String,
    /// The id this crate gave the order, which the venue can be asked about.
    pub client_order_id: String,
    /// `Some` if the venue acknowledged the order before contact was lost.
    pub order_ref: Option<u64>,
}
impl std::error::Error for OrderStateUnknown {}

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

**An `Err` from `execute` means nothing filled, unless it is an `OrderStateUnknown`.** Every `Live`
adapter keeps to this. After a venue accepts an order, a lost response, a failed status query, or a
fill whose details cannot be read (for example commission charged in more than one asset, which
`CexFill` cannot hold) all become `OrderStateUnknown`. None of them may surface as a plain error, and
no fill is ever returned with a guessed commission.

### Required implementations (CEX)

- **`CexLive`** — places a real order against a venue's trading API. Responsible for: rounding
  `quantity` to whatever step/notional size the venue's own rules require *before* sending (an
  unrounded quantity is either rejected outright or, worse, silently accepted and left to drift from
  whatever a paired trade on another venue used); clock synchronisation against the venue's server
  time where the venue requires a timestamp within a tolerance; reading the fill correctly (§ above).
  Configurable by a base URL and a credential set, so that pointing it at a venue's sandbox/testnet
  host with sandbox credentials *is* the "Simulated" mode for this leg (§3) — no separate struct.
  Venues without positions (spot) reject `reduce_only: true` before sending anything.
- **`BinanceFuturesLive`** — the `CexLive` for Binance USDⓈ-M perpetuals (`src/cex/binance_futures/`),
  which sends `reduceOnly`. It checks the account once, at construction, and refuses to return an
  adapter for an account it cannot trade correctly:

  ```rust
  pub struct BinanceFuturesConfig {
      pub base_url: String,   // BINANCE_FUTURES_BASE_URL; defaults to the testnet host, never production
      pub api_key: String,    // BINANCE_FUTURES_API_KEY, required
      pub api_secret: String, // BINANCE_FUTURES_API_SECRET, required
  }

  impl BinanceFuturesLive {
      /// Connects, then refuses to return an adapter for an account it cannot
      /// trade correctly. `symbols` are the only symbols `execute` will accept.
      pub async fn connect(config: BinanceFuturesConfig, symbols: &[&str]) -> Result<Self>;
  }
  ```

  It refuses hedge mode (`dualSidePosition`), multi-asset margin, BNB fee payment (`feeBurn`, which
  would split commission across two assets), a clock too far out to fit `recvWindow`, and any symbol
  that is missing, not `TRADING` or not `PERPETUAL`. Margin type and leverage are read (§6b), never
  set. `execute` rounds `quantity` down to `MARKET_LOT_SIZE`, refuses quantities outside
  `minQty`/`maxQty`, refuses `quantity × quoted_price` below `MIN_NOTIONAL` except for reduce-only
  orders (a small remaining position must always be closable), places a `MARKET` order with its own
  `newClientOrderId`, returns a partial fill (`EXPIRED`/`CANCELED` with `executedQty > 0`) as a
  `CexFill`, reads commission from the order's trades (exactly one asset, or `OrderStateUnknown`), and
  recovers a lost placing response by its client order id. `provenance` is `Landed`, on the testnet
  too.

  Every wait a live CEX adapter makes (request timeout, `recvWindow`, clock refresh, status polling,
  how long trade lines may lag a fill) is one `CexTimings` value, so a test can make them short. A
  failed call is sorted by whether the venue may have acted on it: a connection never opened is not
  sent; a 4xx is a refusal (nothing filled); a timeout, a dropped connection, a 5xx or a 408 is lost,
  and is followed by a status query by client order id rather than a blind resend.
- **`CexStub`** — an in-process fake, same shape and same call-recording requirement as `EvmStub`:
  programmable fills, rejections, partial fills and `OrderStateUnknown`, no network. It records
  `reduce_only`, and has a reduce-only mode: given a signed position set by the test, it rejects any
  reduce-only order that would increase or flip it.

A second venue (a different exchange, with a different wire protocol) gets its own `Live`
implementation; the trait does not change.

## 6b. Reading a perp account

A consumer holding a perp position needs three facts only the venue knows: the position the venue
holds, the account's margin, and the funding it paid or received. Every value below is the venue's own
number, read at call time. Nothing is summed across calls, marked to a price the crate chose, or
remembered (§2).

```rust
// src/cex/account.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginMode { Isolated, Cross }

#[derive(Debug, Clone)]
pub struct PerpPosition {
    pub symbol: String,
    /// Signed: negative is short. Zero when flat, which is an answer and not
    /// an error.
    pub qty: Decimal,
    pub entry_price: Decimal,
    pub mark_price: Decimal,
    /// `None` when flat, or when the venue reports none.
    pub liquidation_price: Option<Decimal>,
    pub margin_mode: MarginMode,
    pub leverage: u32,
    /// `Some` exactly when `margin_mode` is `Isolated`.
    pub isolated_margin: Option<Decimal>,
    /// The venue's own update time for this position, in Unix ms.
    pub as_of_ms: i64,
}

#[derive(Debug, Clone)]
pub struct MarginState {
    /// The asset these figures are in, e.g. "USDT".
    pub asset: String,
    pub margin_balance: Decimal,
    pub maint_margin: Decimal,
    pub available: Decimal,
    pub as_of_ms: i64,
}

#[derive(Debug, Clone)]
pub struct FundingPayment {
    pub symbol: String,
    pub ts_ms: i64,
    /// Signed: positive was received, negative was paid.
    pub amount: Decimal,
    pub asset: String,
    /// The venue's id for this payment, so a caller can de-duplicate.
    pub venue_ref: u64,
}

#[async_trait]
pub trait CexAccount: Send + Sync {
    async fn position(&self, symbol: &str) -> Result<PerpPosition>;
    async fn margin(&self) -> Result<MarginState>;
    /// Every payment with `ts_ms >= since_ms`, oldest first. The adapter pages
    /// through the venue's limit itself; the result is never cut short
    /// without a word.
    async fn funding_since(&self, symbol: &str, since_ms: i64) -> Result<Vec<FundingPayment>>;
    fn label(&self) -> &'static str;
}
```

Reads carry no `Provenance`: nothing is sent, so there is no "sent or thrown away" to report. Which
account was read, testnet or production, is the adapter's `label()` and base URL.

### Required implementations (account reads)

- **`BinanceFuturesAccount`** — shares `BinanceFuturesRest` (client, keys and clock offset) with
  `BinanceFuturesLive`. In one-way mode, which `BinanceFuturesLive` requires, the venue reports one
  position per symbol; more than one row for a symbol is an error, never a sum.
- **`CexAccountStub`** — programmable values and errors; records its calls.

## 7. Contract tests — what keeps the stub honest

A stub that quietly drifts from what `Simulated`/`Live` actually do is worse than no stub: code built
on top of it can pass every test and still be wrong the first time it meets a real venue. This is a
well-known failure mode for mocked dependencies in general, not specific to trading systems — a mock
that is never checked against the real thing eventually stops resembling it, and nothing notices until
production does.

The rule that prevents it: **one shared test suite per port, written generically over "any
`impl DexExecutor`" / "any `impl CexExecutor`" / "any `impl LiquidityExecutor`" / "any
`impl CexAccount`"**, that every implementation must pass:

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

The suites, all in `src/testkit/contract.rs`:

| suite | runs against | asserts |
|---|---|---|
| `dex_executor_contract` | every `DexExecutor` | the two shape rules above |
| `cex_executor_contract` | every `CexExecutor`, with `reduce_only: false` | `order_ref.is_some() == Landed` |
| `cex_spot_rejects_reduce_only` | every spot `CexExecutor` | `reduce_only: true` is an error, and nothing reaches the venue |
| `liquidity_executor_contract` | every `LiquidityExecutor` | one whole life — mint, increase, decrease all of it, collect everything (`u128::MAX` caps), burn — with the §5b shape rules at every step; mint gives a position and liquidity above zero and every later step returns that position; decrease removes exactly what mint and increase added; collect returns, per token, at least what decrease credited; burn succeeds |
| `cex_account_contract` | every `CexAccount` | `isolated_margin.is_some() == (margin_mode == Isolated)`; `qty == 0` implies no liquidation price; funding sorted by `ts_ms`, every `ts_ms >= since_ms`, no repeated `venue_ref`; `asset` never empty |

Run three ways, at different points in the development cycle:

| run against | when | needs |
|---|---|---|
| `Stub` | every test run, unconditionally | nothing |
| `Simulated` (a fork / a sandbox) | before any change to a `Live` adapter ships, and on a recurring schedule even without a code change | fork infrastructure or sandbox credentials. For the liquidity port this is `EvmLiquidity` over a fork sender (§5b) |
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
    ├── evm/                  # the EVM chain family's plumbing, shared by every EVM adapter (§5)
    │   ├── mod.rs
    │   ├── rpc.rs            # EvmRpc: JSON-RPC, eth_call (with `from` and state overrides),
    │   │                     # receipts, revert-reason replay, hex/ABI helpers
    │   ├── erc20.rs          # selectors; balance and allowance reads; storage-slot probing
    │   └── tx.rs             # Signer, EvmSender (signing and fork backends), TxOutcome, FeePolicy
    ├── dex/
    │   ├── mod.rs            # DexExecutor, RouteQuote, SwapRequest, Prepared,
    │   │                     # Realised, Outcome (Provenance is shared, § 4)
    │   └── evm/
    │       ├── live.rs       # EvmLive, over an Arc<EvmSender>
    │       ├── simulated.rs  # EvmSimulated, over an EvmRpc
    │       └── stub.rs       # EvmStub
    ├── liquidity/            # § 5b
    │   ├── mod.rs            # LiquidityExecutor, LiquidityAction, LiquidityRealised, LandedUnread, …
    │   ├── stub.rs           # LiquidityStub
    │   └── evm/
    │       ├── abi.rs        # sol! definitions for both manager ABIs and their events
    │       └── executor.rs   # EvmLiquidity, over an Arc<EvmSender>
    ├── cex/
    │   ├── mod.rs            # CexExecutor, OrderRequest, OrderStateUnknown, CexFill
    │   ├── account.rs        # § 6b: CexAccount, PerpPosition, MarginState, FundingPayment
    │   ├── stub.rs           # CexStub
    │   ├── account_stub.rs   # CexAccountStub
    │   ├── binance/          # shared by spot and futures:
    │   │   ├── sign.rs       #   HMAC-SHA256 query signing
    │   │   ├── clock.rs      #   the venue-clock offset
    │   │   ├── client.rs     #   the signed client; -1021 retry; each failure sorted into
    │   │   │                 #   not sent / refused / lost
    │   │   ├── order.rs      #   finding an order by client order id, trade lines, the
    │   │   │                 #   one-commission-asset rule
    │   │   ├── rest.rs       # spot REST client
    │   │   └── live.rs       # BinanceLive (spot)
    │   ├── binance_futures/
    │   │   ├── rest.rs       # BinanceFuturesRest: signed fapi client, server-clock offset
    │   │   ├── filters.rs    # exchangeInfo → per-symbol MARKET_LOT_SIZE and MIN_NOTIONAL
    │   │   ├── live.rs       # BinanceFuturesLive: impl CexExecutor
    │   │   └── account.rs    # BinanceFuturesAccount: impl CexAccount
    │   └── <venue>/
    │       ├── rest.rs        # a signed REST client for one venue's API
    │       └── live.rs        # CexLive for that venue (base_url selects
    │                           # production vs. sandbox)
    └── testkit/
        └── contract.rs        # § 7's shared suites
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
