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
  nothing is kept between calls and nothing is derived (§6b, §6c);
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

### Ports and venues: one strict contract, venues are adapters

Every port keeps one contract, the same for every venue behind it (`specs/V5-venues-and-solana.md` §1,
after pm-trading's "Fix Venue Abstraction & Enforce Cross-Venue Parity"):

1. **One command interface.** Every action goes through it. The caller never calls a venue directly.
2. **One event interface.** Normalized and deterministic, enough to rebuild the position's state, with
   no venue-specific event.
3. **Explicit capabilities.** The caller branches only on them, never on the venue's name.
4. **The simulator is a venue** like any other, not a special case.
5. **One contract suite.** The same sequence runs against every venue, and asserts the same events
   and the same state changes (§7).

In this crate a **port is the contract**: the DEX port (§5) and the liquidity port (§5b). Its commands,
events and capabilities are the only types a caller sees. A **venue is an adapter behind the port**:
Uniswap v3's position managers, Orca's Whirlpool and Jupiter are venues. A venue's own types (its ABI,
its instructions, its accounts, its program's events) live inside its adapter and never cross the port.
A consumer's own paper simulator implements the same ports and passes the same suite: it is an adapter,
not a mode.

### Simulated runs one of two ways. "Fork" is not a mode

The three modes above are the only modes. Simulated means the real protocol runs and nothing real is
spent, and it runs one of two ways, chosen by what the port needs:

- **A dry run.** One call against the real chain's current state, whose result is thrown away:
  `eth_call` (`EvmSimulated`), `simulateTransaction` (`JupiterSimulated`). This is enough for a swap,
  which is one action.
- **On a fork.** The Live adapter, unchanged, sends to a throwaway copy of the chain: anvil on EVM,
  Surfpool on Solana. The sender's fork backend makes it Simulated (`EvmSender::fork`, §5b;
  `SolanaSender::fork`, §5). This is what a liquidity position needs: its actions build on each other
  (open, then remove, then collect), and a dry run forgets each one's state before the next.

A consumer's paper simulator is neither: its own model of a venue, with no protocol and no chain. It is
a venue like any other, and its outcomes are `Simulated`.

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

```rust
/// The network a command is for. An EIP-155 id means nothing on Solana, so
/// a command names its network instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Network {
    Evm { chain_id: u64 },
    /// Identified by its genesis hash, which the adapter checks against its
    /// node at construction.
    Solana { genesis_hash: [u8; 32] },
}
```

An adapter belongs to one network and refuses a command for any other. `ChainAddress` (§5) stays
bytes: 20 on EVM, 32 on Solana, and the adapter checks the length.

## 5. The DEX port

```rust
use async_trait::async_trait;
use anyhow::Result;
use crate::{Network, Provenance};

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
    pub network: Network,
    pub token_in: ChainAddress,
    pub token_out: ChainAddress,
    pub amount_in: ChainAmount,
    pub expected_amount_out: ChainAmount,
    /// Opaque payload from whatever produced this quote — a route object,
    /// a serialized call, anything the adapter that consumes it knows how
    /// to turn into calldata. Never inspected outside the adapter.
    pub payload: Vec<u8>,
}

/// Who holds a swap's input and pays it to the venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payer {
    /// The sender holds the input, and the address it calls takes it from
    /// the sender: on EVM through an allowance the adapter grants. Every
    /// router and aggregator works this way.
    Sender,
    /// The contract the call is made to holds the input and pays the venue
    /// from it, so the sender grants nothing: a contract that keeps its own
    /// inventory. EVM only; a Solana adapter refuses it.
    CalledContract,
}

/// What a swap's transaction offers to be included ahead of others, above
/// the priority fee its sender's own policy pays (`evm::FeePolicy`,
/// `JupiterConfig::prioritization_fee_lamports`). In the network's own
/// spelling: a price per unit of gas on EVM, lamports on Solana.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PriorityBid {
    /// The sender's policy alone.
    #[default]
    Policy,
    /// This many wei per unit of gas above the policy's priority fee. EVM.
    AbovePolicyPerGas(u128),
    /// This many lamports above the policy's priority fee. Solana.
    AbovePolicyLamports(u64),
}

/// What the caller wants built on top of a `RouteQuote`.
#[derive(Debug, Clone)]
pub struct SwapRequest {
    pub sender: ChainAddress,
    pub recipient: ChainAddress,
    /// Who holds the input: the sender, or the contract the route calls.
    pub payer: Payer,
    /// The minimum acceptable output, as a literal amount. Never a
    /// percentage or basis-point tolerance recomputed at build time — the
    /// caller has already decided the number that makes this worth doing,
    /// and that is the number a revert should be measured against.
    pub min_amount_out: ChainAmount,
    /// Unix timestamp after which the built transaction must not execute.
    /// A swap without one that lands late still lands — a caller that
    /// cares when it lands must set this.
    pub deadline_unix_secs: u64,
    /// What the transaction bids above its sender's fee policy. An adapter
    /// that cannot send a bid refuses one by name (`PriorityBid::policy_only`),
    /// never ignores it: no adapter in this crate sends one yet, and a paper
    /// venue charges it.
    pub priority: PriorityBid,
}

/// A route or command bound to a request: ready to run, one way or
/// another. It passes from a port's `prepare` to its `execute`, and the
/// caller does not read it. An EVM adapter refuses `Prepared::Solana` by
/// name, and the reverse.
#[derive(Debug, Clone)]
pub enum Prepared {
    Evm(EvmCall),
    Solana(SolanaTransaction),
}

/// A call to one contract (what `Prepared` was before it became an enum:
/// the fields are unchanged, so no EVM encoding changes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmCall {
    pub to: ChainAddress,
    pub calldata: Vec<u8>,
    pub value: ChainAmount,
}

/// A transaction built for one fee payer, unsigned until a sender signs it.
#[derive(Debug, Clone)]
pub struct SolanaTransaction {
    pub transaction: solana_transaction::versioned::VersionedTransaction,
    /// The last block height its blockhash is valid at: after it, the
    /// transaction can never land.
    pub last_valid_block_height: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Known never to have landed: the transaction's blockhash has expired
    /// (`last_valid_block_height` is past) and its signature has no status.
    /// Solana can answer this; no EVM adapter returns it.
    Expired,
}

/// What a transaction cost, per family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxCost {
    Evm(EvmCost),
    Solana(SolanaCost),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvmCost {
    /// A receipt's `gasUsed`. For a dry run on `EvmSimulated`, the node's
    /// `eth_estimateGas` of the same call with the same overrides: what the
    /// swap needs, which is at least what it uses. `None` where a dry run
    /// reports none (a revert).
    pub gas_used: Option<u64>,
    pub effective_gas_price_wei: Option<u128>,
    /// A rollup's data fee, from the receipt.
    pub l1_fee_wei: Option<u128>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SolanaCost {
    /// Signature fee + priority fee, from the transaction's meta.
    pub fee_lamports: u64,
    pub units_consumed: u64,
    /// The net change in the lamports of accounts the owner closes (a
    /// position's): positive when rent is locked in them, negative when it
    /// comes back. Over a position's life it sums to the rent still locked,
    /// zero once the position is closed.
    pub rent_deposit_lamports: i64,
    /// The net change in the lamports of accounts that are not the owner's
    /// to close (a tick array): positive when rent is paid into them,
    /// negative when one gives rent back (a dynamic tick array releasing a
    /// tick, whose rent goes into the position). Over a position's life it
    /// sums to the rent the life cost. Per transaction the payer's lamports
    /// move by `−(fee + deposit + spent)`.
    pub rent_spent_lamports: i64,
}

impl TxCost {
    /// The same three figures for every family, in the chain's smallest
    /// native unit (wei, lamports): what a caller records without
    /// branching on the family. On EVM, `deposit` and `spent` are zero, and
    /// `fee` is `gas_used × effective_gas_price + l1_fee`, counting a
    /// missing figure as zero.
    pub fn native(&self) -> NativeCost;
}

/// `deposit` and `spent` are signed: each is the net change in its kind of
/// account, so a sum over any run of transactions is exact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeCost {
    pub fee: u128,
    pub deposit: i128,
    pub spent: i128,
}

#[derive(Debug, Clone)]
pub struct Realised {
    /// `None` exactly when `outcome` is not `Success`. A revert or a
    /// timeout must never be represented as a zero amount — a zero is a
    /// real, terrible price; "no price" is a different fact.
    pub amount_out: Option<ChainAmount>,
    /// The input the swap consumed, which can be less than the route's
    /// `amount_in`: a V3 swap that reaches its price limit takes less than it
    /// was given, and what the pool took is what is booked. `Some` exactly
    /// when `outcome` is `Success`, as `amount_out` is: a swap that reverted,
    /// timed out or expired consumed none that this reports, and no outcome
    /// is represented as a zero input. A live adapter reads it from the
    /// transaction (`EvmLive` from its `Transfer` logs); an adapter that
    /// cannot observe it says what it reports instead (below).
    pub amount_in: Option<ChainAmount>,
    pub outcome: Outcome,
    /// What it cost, whenever something ran, a revert included. A dry
    /// run's figures are what the run reported (an `eth_call` reports no
    /// gas; `simulateTransaction` reports `unitsConsumed`).
    pub cost: TxCost,
    /// The block (or slot) the outcome was observed at.
    pub at: u64,
    pub provenance: Provenance,
    /// Set if and only if a transaction was sent: to the chain (`Landed`) or
    /// to a fork (`Simulated`). A throwaway run sets none.
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

  With `Payer::CalledContract` it reads no allowance and sends no `approve`: the contract called pays
  the venue from its own balance, and the amount out is still the output's `Transfer` to the recipient.
  `prepare` refuses a `RouteQuote` whose `network` is not the sender's. Given a fork sender (§5b)
  instead of a signing one, `EvmLive` becomes a swap simulator whose state persists between calls: its
  provenance is then `Simulated`, and its `tx_ref` is the fork transaction's hash, as a live send's is
  (amended 3 October 2026: a fork send used to set none). Its `cost` is `TxCost::Evm` with the
  receipt's `gasUsed`, `effectiveGasPrice` and, on a rollup, `l1Fee`.
- **`EvmSimulated`** — runs the prepared call against current (or a specified historical) chain state
  through a read-only simulation endpoint (an `eth_call`-equivalent), with no transaction ever
  broadcast. Where the simulated sender does not actually hold the input token or the router's
  allowance, the adapter is responsible for overriding just enough state (balance, allowance) to make
  the call possible — and for granting that allowance *to the router the route actually calls*, not to
  whatever address happens to be probing it. With `Payer::CalledContract` the balance goes on the
  contract called and no allowance is written. A contract not yet deployed is placed by a code override,
  `EvmSimulated::with_code_override(address, CodeOverride { runtime_code, storage })`: its runtime code
  and the storage its constructor would have written, in every call the adapter makes, so a contract
  can be simulated on the chain's real state before it exists. `amount_out` is decoded from the **first 32-byte word**
  of the router's return data: `SwapRouter02.exactInputSingle` returns one word, and KyberSwap's
  `MetaAggregationRouterV2.swap` and the other common aggregator routers put the output amount first.
  Fewer than 32 bytes is an error, never a panic. A router whose output is not its first word is read by a
  return rule set for its address, `EvmSimulated::with_return_rule(address, ReturnRule)`:
  `ReturnRule::FirstWord` (the default), `ReturnRule::LastOfArray`, the last element of one returned
  `uint256[]`, as Uniswap v2's and Aerodrome's `swapExactTokensForTokens` return every hop's amount with
  the amount out last, or `ReturnRule::Word(n)`, the `n`th (from 0) 32-byte word, for a function that
  returns the amount out after something else. Return data the rule cannot decode (an offset or a length
  past the data, a missing or oversized word, an empty array) is an error, never a figure; `EvmLive` needs
  no rule, since it reads the output's `Transfer`. One address can hold functions that return different
  layouts, so a rule can also be set for an address *and* a 4-byte selector, the first 4 bytes of the
  call's calldata: `with_return_rule_for(address, selector, rule)` and
  `with_input_rule_for(address, selector, rule)`. A call is read by the rule for its (address, selector),
  else by the rule set for the address alone (`with_return_rule`, `with_input_rule`, which keep their
  meaning), else by the default; calldata shorter than a selector has no selector's rule.
  With no block given, a run reads the latest block (once, so that its probes and its swap see one state),
  or the node's `pending` block after `EvmSimulated::with_pending_block()`, for a caller that priced from the
  pending state (Base's Flashblocks); a block given to `execute` pins the run to it either way, and
  `Realised::at` is the number of the block read, the number the node gives its pending block for `pending`
  (a node with no pending block is an error). An `eth_call` reports no gas, so for a swap that ran the adapter
  asks the node's `eth_estimateGas` for the same call (same sender, block and overrides), and the
  simulation's `cost` is `TxCost::Evm` with that figure as `gas_used` and every other figure `None`. A swap
  that reverted reports no gas. An `eth_estimateGas` that refuses a swap `eth_call` just ran is an `Err`:
  the node disagreed with itself, and neither answer is the swap's. An `eth_call` shows no transfers, so
  `amount_in` is what the router says it took or else its offer: the dry run gives the payer exactly
  `route.amount_in`, and the amount in is that unless an input rule is set for the address called,
  `EvmSimulated::with_input_rule(address, InputRule::Word(n))` (the `n`th 32-byte word of the return data, as
  `PoolSwapper.swapV3` returns `amountInUsed`; a word that is missing, does not fit a `u128`, or exceeds the
  offer is an error). `InputRule::Offered` is the default, right for a router that spends the exact input it is
  given and a guess for one that can take less without saying so. `EvmLive`'s amount in is the input token's
  `Transfer`s out of the payer (the sender, or the contract called) less any back to it; a landed swap with none
  is an error naming its transaction, as one with no output `Transfer` is.
- **`DexStub`** (today's `EvmStub`, renamed: it was never EVM-specific) — an in-process fake with no
  network calls at all. Must let a test program the exact `Realised` (or error) a given `execute()` call
  returns, including reverts with a specific reason, a forced `TimedOut` and an `Expired`. A programmed
  success takes the whole offer of the route it was prepared from (`program_success`), or the input the
  test names (`program_success_taking`, a swap that reached its price limit). Must record
  every call it received (route, request, prepared value) so a test can assert on what was actually
  sent to it, not just on what it returned.

**Jupiter** is a venue behind this port, with its rules inside its adapters (`src/dex/jupiter/`):

- **The payload** is Jupiter's quote response, verbatim (it is what `/swap` takes back).
- **The minimum.** Jupiter's program takes a slippage in bps against its quoted amount. The adapter
  sets it to the smallest value whose on-chain minimum is at or above `SwapRequest.min_amount_out`,
  rounding toward safety. It refuses a minimum above the quoted amount. The literal number stays the
  contract, and the bps are the venue's encoding of it.
- **The amount out** is the destination token account's balance after, less before. It is read from
  `simulateTransaction`'s returned accounts (dry run), or from the landed transaction's post token
  balances.
- **The amount in** is the quote's `inAmount`, which is `route.amount_in`: an `ExactIn` route (the only mode
  accepted) spends all of it or fails, and slippage is a minimum on the output alone, so a swap that succeeded
  took the whole offer.
- **The cost** is `TxCost::Solana`, carrying the run's `unitsConsumed`.
- **The payer** is the account that signs. Both adapters refuse `Payer::CalledContract` by name.

| Mode | Adapter |
| --- | --- |
| Simulated, dry run | **`JupiterSimulated`**: `/swap`, then `simulateTransaction` with `sigVerify: false` and `replaceRecentBlockhash: true`, from an account that holds the input token (Solana has no state override) |
| Simulated, on a fork | **`JupiterLive`** over a fork `SolanaSender` (Surfpool) |
| Live | `JupiterLive` over a signing `SolanaSender`: not built until the owner asks |
| Stub | `DexStub` |

`Stub` is chain-family-agnostic and is not duplicated per chain. The trait does not change for a
second family: only `Prepared`, `Outcome` and `TxCost` gained the Solana forms above.

### Resolving a swap left unknown

An `EvmLive` send that ends `Outcome::TimedOut` is not over. `EvmLive` keeps what it needs to read the
swap's result by the transaction's hash, until the chain has decided it, and a caller asks again:

```rust
// src/dex/mod.rs
/// What became of a swap whose adapter gave up waiting, as the chain says now.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// Not decided: the node still knows the transaction, or its nonce is unused and another node may yet
    /// broadcast it. Ask again.
    Pending,
    /// It ended: mined, a revert included. A success carries `amount_in` and `amount_out` read exactly as a
    /// sent swap's are (what the pool took is what is booked).
    Done(Realised),
    /// Replaced or dropped, and did not land.
    Gone,
}

// src/dex/evm/live.rs
impl EvmLive {
    /// `tx_hash` is the hash an `execute` of this adapter ended `TimedOut` with (`Realised.tx_ref`).
    pub async fn resolve(&self, tx_hash: B256) -> Result<Resolution>;
}
```

- `Gone` is only for a transaction the node no longer knows whose nonce has been used. `EvmSender::resolve`
  returns an `Err` for that and for a failed RPC read; the first is a `TxReplacedOrDropped` (found with
  `downcast_ref`, with the message it always had), the second leaves the sender's latch intact and is an `Err`
  of `EvmLive::resolve` too. A failed read is never `Gone`.
- A hash this adapter did not time out is an `Err`. Resolve through `EvmLive`, not through the sender: the
  sender's `resolve` clears its latch and hands the outcome to its caller, after which `EvmLive::resolve` of
  that hash is an `Err`.
- `Done`'s amounts come from the same transfer-log decoders `execute` uses. A swap that landed with logs they
  cannot read is an `Err`, as from `execute`, and one answer only: it is decided, so it is forgotten, and
  `resolve` of that hash again is an `Err` saying there is nothing to resolve.
- A swap's context is kept in memory only: after a restart there is nothing to resolve through.

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
    /// Checks `eth_chainId` against `chain_id`, and reads `web3_clientVersion` once: a node that reports
    /// `anvil` makes this sender `Simulated`. Returns an error if this process already holds an
    /// `EvmSender` for the same (address, chain_id).
    pub async fn connect(rpc: EvmRpc, signer: Signer, chain_id: u64, fees: FeePolicy) -> Result<Arc<Self>>;
    pub fn address(&self) -> Address;
    pub fn chain_id(&self) -> u64;
    /// `Simulated` for a fork sender, and for a signing sender whose node is anvil; `Landed` otherwise.
    pub fn provenance(&self) -> Provenance;

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
    /// hash (a `TxReplacedOrDropped`), and the hash is cleared. A failed read is another error and
    /// leaves the hash.
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
- **A fork never reads as a landing.** `connect` reads `web3_clientVersion` once and keeps the answer. A
  node whose version starts with `anvil` makes the sender `Simulated`, so every outcome an adapter reports
  through it says so, and a fork run signed with a real key cannot be read as a mainnet trade. A node that
  answers that it has no such method (`-32601`) is not anvil, which has it. Any other answer that is not a
  version (another error object, a rate limit or a gateway's refusal included, or no answer at all) is an
  error, because the sender could not say what it sends to: it does not guess `Landed`. A signing sender
  still never writes a balance (`ensure_balance`), anvil included.
- **An adapter's label follows its sender's provenance.** `EvmLive` is `"evm-live"` or `"evm-live-fork"`,
  `EvmLiquidity` `"evm-liquidity-live"` or `"evm-liquidity-fork"`: a signing sender on anvil is `Simulated`, so
  it carries the `-fork` label, as a fork sender does. Nothing in the crate branches on a label.
- **The sender prices a transaction by its `FeePolicy` alone.** It takes no priority bid: `EvmLive` refuses
  `PriorityBid::AbovePolicyPerGas` above zero, by name, over a signing sender and a fork sender alike.
- **No convenience constructor builds a sender inside an adapter.** It would make a second sender for
  the same wallet the easy thing to write.

The EVM chain family's plumbing is shared by every EVM adapter rather than copied into each:
`src/evm/rpc.rs` (`EvmRpc`: JSON-RPC, `eth_call` with `from` and state overrides, receipts,
revert-reason replay, hex/ABI helpers) and `src/evm/erc20.rs` (selectors, balance and allowance reads,
storage-slot probing). The probing is one prober for every adapter: a token's `balanceOf` and `allowance`
mappings are found once per token, in one `eth_call` each, by writing a sentinel of its own into the slot
each candidate base would put the entry at and reading back which one the token returns. The candidates are
the first 24 storage slots and OpenZeppelin v5's ERC-7201 namespace for its upgradeable ERC-20
(`erc7201:openzeppelin.storage.ERC20`, where MORPHO on Base keeps its state: the balances at the namespace,
the allowances one slot on, both checked against the storage `ERC20Upgradeable` v5.6.1 and v5.7.0 write). A
token that keeps them anywhere else is an error naming what was probed, and a node that did not answer is an
error, never "no candidate".

### The Solana family — one sender per wallet and cluster

The Solana family's plumbing lives in `src/solana/`, shared by every Solana adapter (Jupiter here,
Whirlpool in §5b), as `src/evm/` is for EVM. Transactions, keys and instructions are the Solana SDK's
own crates — `solana-pubkey` (or `solana-address`), `solana-hash`, `solana-instruction`,
`solana-message`, `solana-transaction`, `solana-keypair`, `solana-signer` — on one version line,
pinned. The crate does not use `solana-rpc-client`.

```rust
// src/solana/rpc.rs
/// JSON-RPC over `reqwest`, as `EvmRpc` is: `getGenesisHash`,
/// `getLatestBlockhash`, `getBlockHeight`, `simulateTransaction`,
/// `sendTransaction`, `getSignatureStatuses`, `getTransaction`,
/// `getMultipleAccounts`, and the fork node's `surfnet_*` methods.
pub struct SolanaRpc { /* reqwest client, URL */ }

// src/solana/tx.rs
pub enum SolanaTxOutcome {
    Success { slot: u64, signature: [u8; 64], cost: SolanaCost, meta: TxMeta },
    /// The transaction landed and failed: its fee is spent, its effects are not.
    Failed { slot: u64, signature: [u8; 64], reason: String, cost: SolanaCost },
    /// Sent, not seen, and its blockhash not yet expired when polling gave up.
    TimedOut { signature: [u8; 64] },
    /// Its blockhash expired with no status for its signature: it can never land.
    Expired { signature: [u8; 64] },
}

/// The only thing in the crate that signs for one wallet on one cluster.
/// Every Solana adapter that sends from that wallet holds the same `Arc`.
pub struct SolanaSender { /* SolanaRpc, backend (signing or fork), network, poll settings */ }

impl SolanaSender {
    /// A signing sender, with a key from the environment. **Refused**: it
    /// returns an error until the owner asks for Live.
    pub async fn signing(rpc: SolanaRpc, network: Network) -> Result<Arc<Self>>;
    /// A sender on a Surfpool fork. It refuses any node that does not serve
    /// a `surfnet_*` method (a real cluster serves none), generates its key
    /// at run time and never writes it, and funds it with SOL for fees.
    pub async fn fork(rpc: SolanaRpc) -> Result<Arc<Self>>;
    pub fn address(&self) -> [u8; 32];
    pub fn network(&self) -> Network;
    /// `Simulated` for a fork sender, `Landed` for a signing one.
    pub fn provenance(&self) -> Provenance;
    pub fn rpc(&self) -> &SolanaRpc;
    /// A fresh blockhash and the block height it stays valid to, for an
    /// adapter building a `SolanaTransaction`.
    pub async fn latest_blockhash(&self) -> Result<([u8; 32], u64)>;
    /// Sign, send, and poll the signature until its outcome is known or
    /// its blockhash expires.
    pub async fn send_and_confirm(&self, tx: &SolanaTransaction) -> Result<SolanaTxOutcome>;
    /// Checks that the owner's token account for `mint` holds at least
    /// `amount`. A signing sender returns an error when it does not; a fork
    /// sender writes the balance with Surfpool's cheatcode, as
    /// `EvmSender::ensure_balance` writes a storage slot on anvil.
    pub async fn ensure_balance(&self, mint: [u8; 32], token_program: [u8; 32], amount: u64) -> Result<()>;
}
```

Rules, as for `EvmSender`:

- **One `SolanaSender` per (address, network) per process**, and adapters refuse actions for any other
  network. A sender checks `getGenesisHash` against its network at construction.
- **The fork backend never sends anywhere real**: it refuses a node that does not answer a `surfnet_*`
  method, as the EVM fork backend refuses a node that is not anvil.
- **A send that ends `TimedOut`** blocks the next send until it is resolved, as on EVM. **`Expired` is
  terminal**: the transaction can never land, so it blocks nothing.
- **A fork never runs a transaction against an account newer than its clock.** Surfpool fetches an
  account from its upstream the first time a transaction needs it, so the account carries the chain's
  time at that moment, while the fork's clock runs from its own start and is about a second behind
  the chain's. A program that orders the two refuses (a Whirlpool: "Timestamp should be greater than
  the last updated timestamp"). So a fork send first loads the transaction's accounts with a dry run,
  and sends once the fork's clock has passed the second the dry run ended in, waiting no longer than
  the sender's poll timeout; otherwise it is an `Err` with nothing sent. (Surfpool 1.6.0's
  `surfnet_timeTravel` cannot do this instead: it writes the slot's index within the epoch into the
  `Clock` sysvar's absolute slot, so every lookup table's entries read as not yet active, and the
  clock falls back behind at the next slot.)
- **Pace the public endpoints.** Surfpool fetches mainnet accounts on demand from its upstream RPC;
  one Solana process per IP on the public endpoints. A swap needs a burst of reads that the free
  nodes refuse with HTTP 429, which Surfpool reports as a bare "Internal error" (and a send with
  preflight skipped is then dropped, so it ends `Expired`). `scripts/rpc-pacer.py` sits between the
  fork and its upstream, one call at a time with 429s retried; with `--split-accounts` it sends a
  `getMultipleAccounts` as one `getAccountInfo` per key, which Solana Vibe Station's public node
  needs (more than 3 keys a call are always refused).

## 5b. The liquidity port

A concentrated-liquidity position is opened, grown, shrunk, harvested and closed. `DexExecutor` cannot
express this: `Realised` holds one `amount_out`, while an open returns a position, an amount of
liquidity and two token amounts. The port turns a decided command into a known outcome like the other
two; it has its own trait because its outcomes have a different shape. It is one contract for every
venue (§3): Uniswap v3's position managers on EVM, Orca's Whirlpool on Solana, and a consumer's paper
model pass the same suite.

In this port's terms, the crate does **not**: choose ranges, or compute liquidity from prices or
prices from ticks (the venue computes the liquidity from a deposit in token amounts, the chain reports
it, and the crate reads the report back); value a position, track what it holds, or remember what it
opened; stake positions in gauges or claim emissions; create pools; wrap native ETH or SOL (tokens are
ERC-20s or SPL tokens); or create the owner's token accounts on Solana. It does not support
fee-on-transfer tokens: the EVM adapter detects them and refuses to read their outcome, but does not
handle them.

### Commands

```rust
// src/liquidity/mod.rs
use crate::dex::{ChainAddress, Outcome, Prepared, TxCost};
use crate::{Network, Provenance};

pub enum LiquidityCommand {
    /// Open a position on `range` and deposit into it.
    Open { range: Range, deposit: Deposit },
    /// Deposit more into an open position.
    Add { position: PositionId, deposit: Deposit },
    /// Take `liquidity` out of the range, refusing less than `min_out` of each token.
    Remove { position: PositionId, liquidity: u128, min_out: TokenPair },
    /// Transfer to the owner everything the position owes.
    Collect { position: PositionId },
    /// Close a position that holds no liquidity and owes nothing.
    Close { position: PositionId },
}

/// The pool, not a manager or a program: the adapter knows its venue's
/// deployment from its construction, and reads the pool's tokens and key
/// from the pool.
pub struct Range { pub network: Network, pub pool: ChainAddress, pub tick_lower: i32, pub tick_upper: i32 }

pub struct Deposit { pub max: TokenPair, pub guard: DepositGuard }

/// The slippage guard. A venue enforces one kind, and says which in its
/// capabilities.
pub enum DepositGuard {
    MinAmounts(TokenPair),
    SqrtPriceBand { min_sqrt_price_x64: u128, max_sqrt_price_x64: u128 },
}

/// Opaque to the caller: an ERC-721 id under a manager (32 big-endian
/// bytes), a position mint under a program.
pub struct PositionId { pub network: Network, pub bytes: Vec<u8> }

/// In the pool's own token order.
pub struct TokenPair { pub token0: u128, pub token1: u128 }

pub struct LiquidityRequest {
    /// Signs and pays. It also receives the position and every token the
    /// position transfers: the crate never sends either anywhere else.
    /// Never the zero address.
    pub owner: ChainAddress,
    /// Unix time after which the command must not execute. Required, as for
    /// a swap.
    pub deadline_unix_secs: u64,
}
```

Both venues take a deposit in token amounts and compute the liquidity themselves: Uniswap's managers in
`mint` and `increaseLiquidity`, Whirlpool in `increase_liquidity_by_token_amounts_v2`, which takes
`token_max_a`, `token_max_b` and a sqrt-price band. `Collect` has no caps: it always takes everything.

### Events and reports

```rust
pub enum LiquidityEvent {
    Opened { position: PositionId, liquidity: u128, paid: TokenPair },
    Added { liquidity: u128, paid: TokenPair },
    /// `released`: the principal taken out of the range. `transferred`:
    /// what reached the owner in this transaction.
    Removed { liquidity: u128, released: TokenPair, transferred: TokenPair },
    Collected { transferred: TokenPair },
    Closed,
}

/// One per command executed.
pub struct LiquidityReport {
    pub outcome: Outcome,
    /// `Some` exactly when `outcome` is `Success`.
    pub event: Option<LiquidityEvent>,
    /// Whenever something ran, a revert included.
    pub cost: TxCost,
    /// The block or slot the outcome was observed at.
    pub at: u64,
    pub provenance: Provenance,
    /// `Some` exactly when a transaction was sent: to the chain (`Landed`)
    /// or to a fork (`Simulated`).
    pub tx_ref: Option<Vec<u8>>,
}

/// Returned inside `anyhow::Error` when a command's transaction landed but
/// its outcome could not be read: an expected event was missing, or the
/// cross-check failed. Something happened on chain, so the caller must
/// inspect `tx_ref` before it acts on this position again.
pub struct LandedUnread { pub tx_ref: Vec<u8>, pub reason: String }
impl std::error::Error for LandedUnread {}
```

The events state facts, never a venue's semantics. **A position's token flow is the sum of `paid`, less
the sum of `transferred`. The fees it earned are the sum of `transferred`, less the sum of
`released`.** The caller computes both the same way for every venue.

### Capabilities and the trait

```rust
pub enum DepositGuardKind { MinAmounts, SqrtPriceBand }

pub struct LiquidityCapabilities {
    /// The guard the venue enforces on a deposit.
    pub deposit_guard: DepositGuardKind,
    /// A Remove transfers the principal at once. Otherwise it stays owed until Collect.
    pub remove_transfers: bool,
    /// Opening a range may create accounts whose rent the owner cannot close (`NativeCost.spent`).
    pub open_may_spend_rent: bool,
    /// Close returns a deposit (a negative `NativeCost.deposit`).
    pub close_returns_deposit: bool,
}

#[async_trait]
pub trait LiquidityExecutor: Send + Sync {
    fn capabilities(&self) -> LiquidityCapabilities;
    /// Validates the command and encodes it. It may read the chain (the
    /// pool, the position) but sends nothing.
    async fn prepare(&self, cmd: &LiquidityCommand, req: &LiquidityRequest) -> Result<Prepared>;
    /// Runs the prepared command to a terminal outcome and does not return
    /// before then, like `DexExecutor::execute`.
    async fn execute(&self, prepared: &Prepared) -> Result<LiquidityReport>;
    fn label(&self) -> &'static str;
}
```

The caller branches on `deposit_guard` alone, to build the guard. The other three explain the events'
and costs' figures, and no accounting needs them.

**There is no `at` parameter on `execute`.** A liquidity command changes state that later commands
depend on, so re-running one alone at an older block means nothing. A caller who wants an older state
starts the fork at that block.

### The rules every venue keeps

Each holds on every venue, and `liquidity_executor_contract` (§7) checks it:

- **A command the position cannot take is refused before sending**, an `Err` with nothing sent.
  Removing more liquidity than the position holds, closing a position that holds liquidity or owes
  tokens, and acting on a position the owner does not hold are refused this way. Every venue reads the
  position first.
- **Collect on a position that owes nothing** is a success that transfers zero.
- **A passed deadline** is refused without sending. On Solana, the blockhash also bounds the
  transaction once sent (`Outcome::Expired`).
- **A transaction that ran and whose effect cannot be read** is `LandedUnread`, on every venue.
- **The shape rules**: `event` is `Some` exactly on `Success`; `tx_ref` is `Some` exactly when a
  transaction was sent, to the chain or to a fork; `cost` is set whenever something ran. An `Err`
  means nothing was sent for the command itself, unless it is `LandedUnread`. A failed approval is an
  `Err`: it is setup, and no position changed.

### The venues

| | Uniswap v3 position managers (EVM) | Orca Whirlpool (Solana) | A consumer's paper model |
| --- | --- | --- | --- |
| Deployment given at construction | the manager's address and its ABI variant (Uniswap, PancakeSwap, Slipstream) | the Whirlpool program id | the pool's mirror |
| `Open` | `mint` | the range's missing tick arrays created, then `open_position_with_token_extensions` and `increase_liquidity_by_token_amounts_v2`, in one transaction | the position maths |
| `Add` | `increaseLiquidity` | `increase_liquidity_by_token_amounts_v2` | the position maths |
| `Remove` | `decreaseLiquidity`: the principal stays owed | `decrease_liquidity_v2`: the principal is paid at once | as its venue would |
| `Collect` | `collect`: owed principal and fees | `update_fees_and_rewards`, then `collect_fees_v2` | what the position owes |
| `Close` | `burn` | `close_position_with_token_extensions`: the position's rent comes back | — |
| Events read from | the manager's logs, checked against the ERC-20 `Transfer` logs | the program's own events (`LiquidityIncreased`, `LiquidityDecreased`, `PositionOpened`), checked against the token accounts' balance changes | itself |
| `deposit_guard` | `MinAmounts` | `SqrtPriceBand` | either, as configured |
| `remove_transfers` | false | true | as its venue |
| `open_may_spend_rent`, `close_returns_deposit` | false, false | true, true | as its venue |
| Live, Simulated | `EvmLiquidity` over a signing sender, or over anvil | `WhirlpoolLiquidity` over a signing `SolanaSender` (not built until the owner asks), or over Surfpool | Simulated only |

Inside the Whirlpool adapter, and never outside it: the instructions above, the position mint and its
derived account, the tick arrays (fixed or dynamic), SPL Token and Token-2022 (the `_v2` instructions
take either), and the rent figures it reports in `SolanaCost`.

### Required implementations (liquidity)

A position's life is a sequence of commands, each depending on the state the last one left, and a dry
run discards that state. So Simulated runs the commands **on a fork**, as real transactions: the same
operation as Live, sent to a chain that is thrown away afterwards (§3). One implementation, two senders,
and Simulated and Live share every line of encoding and decoding.

- **`LiquidityStub`** — no network calls. Each call's outcome is programmed: success with a given
  event and cost, `Reverted { reason }`, `TimedOut`, an `Err`, or `LandedUnread`. Its capabilities are
  set by the test. It records every `prepare` and `execute` it receives. **A command with no programmed
  outcome is an `Err` naming the command**: a fallback would have to invent liquidity and amounts.
- **`EvmLiquidity`** — one implementation over an `Arc<EvmSender>`; its label is
  `"evm-liquidity-live"` or `"evm-liquidity-fork"`, from the sender.

  ```rust
  /// Which position manager ABI the deployment speaks.
  pub enum ManagerAbi {
      /// Uniswap v3's NonfungiblePositionManager: the pool key is the fee tier.
      UniswapV3,
      /// PancakeSwap v3's: ABI-identical to Uniswap v3's.
      PancakeSwapV3,
      /// Aerodrome and Velodrome Slipstream: the pool key is the tick spacing, and
      /// `mint` ends with `sqrtPriceX96`, always sent as zero (this crate never
      /// creates a pool).
      Slipstream,
  }

  impl EvmLiquidity {
      pub fn new(sender: Arc<EvmSender>, manager: Address, abi: ManagerAbi) -> Self;
  }
  ```

  Its encoding, its checks and its log reading are V3's; only where its inputs come from changed.
  `prepare` checks the owner is the sender's address and not zero, the deadline is set and has not
  passed, and the command's `network` is the sender's. `Open`: tick_lower < tick_upper, the deposit's
  maxima are not both zero, the guard is `MinAmounts` with each minimum at most its maximum; it reads
  `token0`, `token1` and the pool key (`fee`, or `tickSpacing` for Slipstream) from the pool, and
  refuses a pool whose `factory` is not the manager's (tick spacing is the manager's to check). Any
  other command: `ownerOf(id)` must be the owner, and `positions(id)` gives the tokens, the liquidity and
  what is owed — `Remove` above the liquidity is refused, as is `Close` on a position holding liquidity
  or owing tokens. Encoding uses `alloy-sol-types`, never hand-encoding.

  `execute`: refuses a passed deadline without sending; for `Open` and `Add`, `ensure_balance` and
  `ensure_allowance` per token, with the manager as spender and **exactly the deposit's maximum**, never
  `U256::MAX`; sends to the manager; decodes the events from the manager's own logs (`LandedUnread` if
  one is missing); **cross-checks** against the ERC-20 `Transfer` logs of the same receipt — for `Open`
  and `Add` the owner sent exactly `paid`, for `Collect` it received exactly `transferred`, zero amounts
  skipped — and gives `LandedUnread` naming both figures on a mismatch; an amount above `u128` is
  `LandedUnread`, never truncated. `Remove`'s `transferred` is zero. `Reverted` and `TimedOut` map
  onto `Outcome` as they do for swaps, and `cost` is the receipt's, as for `EvmLive`.

  Manager addresses are not listed in the crate: the consumer passes them in. Uniswap v4's
  `PositionManager` (`modifyLiquidities` with Permit2) is deferred until a consumer needs it.
- **`WhirlpoolLiquidity`** — one implementation over an `Arc<SolanaSender>`, constructed with the
  Whirlpool program id. Over a fork sender (Surfpool) it is Simulated; over a signing sender it would be
  Live, which is not built until the owner asks. `Open` creates the range's missing tick arrays in the
  same transaction (dynamic tick arrays, `initialize_dynamic_tick_array`). Rent is signed, one figure per
  kind of account: the net change in the tick arrays is `rent_spent_lamports`, and in the position's
  three accounts `rent_deposit_lamports`; `Close` reports the position's rent as a negative deposit. A
  dynamic tick array gives a tick's rent back into the position when `Remove` releases the tick (spent
  down, deposit up by the same, in one report), and `Close` returns it with the rest. So over a life the
  deposits sum to zero and the spent rent is what the arrays kept. Measured on Surfpool, 1 October 2026,
  on a new array (deposit / spent, lamports): `Open` +6,765,120 / +3,480,000, `Remove` +1,559,040 /
  −1,559,040, `Collect` 0 / 0, `Close` −8,324,160 / 0; spent 1,920,960 over the life, the empty array's
  rent. On existing arrays: `Open` +8,324,160 / 0 and `Close` −8,324,160 / 0. Amounts are read from the
  program's events and checked against the owner's token accounts' balance changes; the rent figures
  are checked against the owner's lamports (they moved by exactly `−(fee + deposit + spent)`). Either
  mismatch is `LandedUnread`. A position is named by its mint; `Open` mints it
  under Token-2022 with a key generated at `prepare`, which co-signs the transaction. `Collect` sends
  `update_fees_and_rewards` only while the position holds liquidity (the program refuses it on an empty
  one, whose fees the `Remove` brought up to date). Not supported: a Token-2022 mint with a transfer
  hook, and closing a position minted under SPL Token.

The EVM sender gains a fork backend for this mode:

```rust
impl EvmSender {
    /// A sender that impersonates `owner` on an anvil fork. It refuses any
    /// node whose `web3_clientVersion` does not start with "anvil": this
    /// backend sends transactions, and must never send them anywhere real.
    pub async fn fork(rpc: EvmRpc, owner: Address, chain_id: u64) -> Result<Arc<Self>>;

    /// `Simulated` for a fork sender, `Landed` for a signing one. Adapters
    /// take their provenance from this. Either way a transaction was sent,
    /// so they set `tx_ref` to its hash.
    pub fn provenance(&self) -> Provenance;

    /// Checks that `holder` holds at least `amount` of `token`: the owner,
    /// or a contract that pays a swap from its own inventory (a swap's
    /// payer). A signing sender returns an error when it does not, so a
    /// transaction that would revert with STF is never sent. A fork sender
    /// instead writes the holder's balance slot with `anvil_setStorageAt`
    /// (the slot comes from `evm::erc20`'s probing), exactly as
    /// `EvmSimulated` overrides state.
    pub async fn ensure_balance(&self, token: Address, holder: Address, amount: U256) -> Result<()>;
}
```

On a fork, `send_and_confirm` calls `anvil_impersonateAccount` and then `eth_sendTransaction` from the
owner, and follows the same receipt polling, revert-reason replay, timeout path and unresolved-timeout
rule as signing. One difference is deliberate: when the node's gas estimate says a transaction will
revert, a signing sender refuses to spend gas on it (an `Err`, nothing sent), while a fork sender
sends it anyway with a fixed gas limit, so a fork run observes the revert — `"Price slippage check"` —
as an `Outcome::Reverted` with its reason. (`"Not cleared"` is no longer reachable: `Close` on a
position that holds liquidity or owes tokens is refused before sending.) A fork sender is not in the process-wide
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
    /// The id this crate gave the order. Set if and only if `Landed`.
    pub client_order_id: Option<String>,
    /// The venue's own time for the order, ms since the epoch. `None` when nothing was sent.
    pub venue_time_ms: Option<u64>,
    /// The order's trades as the venue listed them; empty when nothing was sent, or where the
    /// adapter does not read them. When listed, their quantities add up to `filled_qty`.
    pub trades: Vec<CexTrade>,
}

pub struct CexTrade {
    pub trade_id: Option<u64>, // Binance: the public trade stream's id for the same trade
    pub price: Decimal,
    pub qty: Decimal,
    pub commission: Decimal,
    pub commission_asset: String,
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

**Every `Err` from `execute` carries the provenance of the order it is the failure of**: the one a fill
from that executor would carry, `Landed` for a live adapter (a refusal before anything was sent included)
and `Simulated` for a stub or a paper model. It is a layer of the error's chain, `ErrorProvenance`,
found with `downcast_ref` or read with `cex::provenance_of(&err)`, and attached with
`cex::with_provenance(err, provenance)`: an error that already carries one keeps it. The layer's
`Display` is the message of the layer it covers, so an error's `to_string()` is what it was without it,
and whatever else is in the chain, `OrderStateUnknown` included, is still found by `downcast_ref`.
`OrderStateUnknown` itself gains no field, so a caller that builds one by struct literal is unaffected;
it attaches the provenance with `with_provenance` when it returns the error. (`{:#}` of an error tagged
after the fact says its top-level message twice; an `OrderStateUnknown` this crate builds does not.)

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

  **What a fill names (amended 3 October 2026).** A landed `CexFill` names the client order id it
  was sent with, the venue's time for it and, where the adapter reads them, its trades:

  | Adapter | `venue_time_ms` | `trades` |
  | --- | --- | --- |
  | `BinanceLive` (spot) | `transactTime`; `updateTime` when the order was found by a status query | the `fills` of the `FULL` answer, or `myTrades`; each with its `tradeId` (`id` there) |
  | `BinanceFuturesLive` | the order's `updateTime` | `userTrades`, each with its `id` |
  | `BybitLive` | the order's `updatedTime` | none: it reads the order, not its executions |

  A stub or a paper model sends nothing, so its fill names no client order id, no venue time and no
  trades. `testkit::contract::assert_fill_shape` asserts these rules, and `cex_executor_contract`
  runs it on every adapter it is given.

  **`BinanceLive::test_order`** checks an order without placing it: the same `MARKET` request,
  rounded and refused as `execute` would, sent to `POST /api/v3/order/test` with
  `computeCommissionRates=true`. The answer is an `OrderCheck`: the quantity checked, the client
  order id it carried, and the `CommissionRates` Binance states for that order (standard, special
  where given, tax, and the BNB discount), each a fraction of the traded amount. Nothing reaches the
  matching engine, so a test order has no fill, no order id and no provenance, and any failure,
  a lost answer included, is a plain error. It is what a shadow stage calls.

  Both are checked on the spot testnet by the ignored `binance_spot_testnet`, which refuses any other
  host. Run on 3 October 2026 (AEROUSDT): the test endpoint accepted 7.6 AERO and refused 1.3 with
  `-1013 Filter failure: NOTIONAL`; a market buy and a market sell of 7.6 each filled in one trade,
  naming its client order id, `transactTime` and trade id. The testnet charges nothing and answers
  `"discountAsset": null`, which the documentation does not show, so `CommissionDiscount.asset` is
  optional.

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

### Resolving an order left unknown

An `OrderStateUnknown` names an order the venue may have filled. `CexOrders` asks the venue again, by the
client order id the adapter itself gave it:

```rust
// src/cex/orders.rs
#[derive(Debug, Clone)]
pub enum OrderState {
    /// Ended `FILLED`: the whole fill.
    Filled(CexFill),
    /// Ended with part filled and the rest not (`EXPIRED`, `CANCELED`, `EXPIRED_IN_MATCH`: a market order
    /// that ran out of book, for example). What `execute` returns as a partial `CexFill`.
    PartlyFilled(CexFill),
    /// Ended with nothing filled: the venue accepted the order and it ended in `status`.
    Rejected { order_ref: u64, status: String },
    /// The venue has no such order, and can no longer accept the request that placed it: nothing filled.
    NotFound,
    /// Not settled: still working, or the venue does not know it yet and could still accept the lost
    /// request. Ask again.
    Open,
}

#[async_trait]
pub trait CexOrders: Send + Sync {
    async fn order_state(&self, symbol: &str, client_order_id: &str) -> Result<OrderState>;
}
```

- **One read, no waiting.** The same status query `execute` follows an order with, once, and the trade lines
  of a fill read as `execute` reads them. The caller sets the schedule.
- **`Err` is a read that failed.** Ask again. It is never `NotFound`.
- **`NotFound` is conclusive only after `recvWindow` and the clock's error bound have passed** since the
  request that placed the order was signed. `BinanceLive` remembers that moment for each placing request whose
  answer it lost, until the order's state has been read; "no such order" before then is `Open`. For an id it
  holds no lost request for, "no such order" is `NotFound` at once.
- No caller chooses a client order id, so a resolver cannot turn a resend into a first send; Binance refuses a
  reused id only while the first order is open.
- `BinanceLive` and `CexStub` implement it. `BinanceFuturesLive` and `BybitLive` do not. It has no `label()`:
  both already have one from `CexExecutor`, and a second would make `adapter.label()` ambiguous wherever both
  traits are in scope.

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

## 6c. Reading balances

A consumer that keeps its own books reconciles them against what a venue holds. The `BalanceReader` port is
one trait per kind of account: the figure the venue reports, at the moment (and, on EVM, the block) it is
asked, exact. Nothing is cached, summed or valued (§2).

```rust
// src/balance/mod.rs
#[async_trait]
pub trait EvmBalanceReader: Send + Sync {
    /// `address`'s native balance in wei at `block`.
    async fn native(&self, address: Address, block: BlockTag) -> Result<ChainAmount>;
    /// `holder`'s `balanceOf` of `token` at `block`.
    async fn token(&self, token: Address, holder: Address, block: BlockTag) -> Result<ChainAmount>;
    fn label(&self) -> &'static str;
}

pub struct SpotBalance { pub asset: String, pub free: Decimal, pub locked: Decimal }

pub struct SpotAccountBalances {
    /// Only assets the account holds some of: an asset that is absent is held at zero.
    pub balances: Vec<SpotBalance>,
    /// The venue's time of the account's last change (`updateTime`), in Unix ms.
    pub update_time_ms: Option<u64>,
}

#[async_trait]
pub trait SpotBalanceReader: Send + Sync {
    async fn balances(&self) -> Result<SpotAccountBalances>;
}
```

`SpotBalanceReader` has no `label()`, for the reason `CexOrders` has none: `BinanceLive` already has one.

- An amount above `u128` is an error naming the token and holder, never truncated.
- A read at a past block needs a node that serves that state (an archive node, or a fork that holds the
  history); a node that cannot is an `Err`, never the latest value.
- `locked` is what an open order holds: still the account's.
- Reads carry no `Provenance`: nothing is sent.

### Required implementations (balance reads)

- **`EvmBalances`** — over an `EvmRpc`: `eth_getBalance` and `balanceOf` at the block named
  (`EvmRpc::balance_at`; `balance` is `Latest`).
- **`BinanceRest`, `BinanceLive`** (spot) — `GET /api/v3/account?omitZeroBalances=true`, through the signed
  client, so the clock and the error rule are the order path's.
- **`EvmBalanceStub`, `SpotBalanceStub`** — programmable values and errors; the EVM stub records its calls and
  the spot stub counts them. The EVM
  stub answers from a history (a value set at block `b` holds until a later block sets another); a read before
  the first value is an error, never zero.

## 7. Contract tests — what keeps the stub honest

A stub that quietly drifts from what `Simulated`/`Live` actually do is worse than no stub: code built
on top of it can pass every test and still be wrong the first time it meets a real venue. This is a
well-known failure mode for mocked dependencies in general, not specific to trading systems — a mock
that is never checked against the real thing eventually stops resembling it, and nothing notices until
production does.

The rule that prevents it: **one shared test suite per port, written generically over "any
`impl DexExecutor`" / "any `impl CexExecutor`" / "any `impl LiquidityExecutor`" / "any
`impl CexAccount`" / "any `impl EvmBalanceReader`" / "any `impl SpotBalanceReader`" / "any
`impl CexOrders`"**, that every implementation must pass:

```rust
// sketch — the real suite lives in `src/testkit/contract.rs`
pub async fn dex_executor_contract(executor: &dyn DexExecutor, sends: Sends, fixture: ContractFixture) {
    let prepared = executor.prepare(&fixture.route, &fixture.request).await.unwrap();
    let realised = executor.execute(&prepared, None).await.unwrap();

    // Shape assertions every implementation must satisfy, regardless of
    // mode:
    assert_eq!(realised.amount_out.is_some(), matches!(realised.outcome, Outcome::Success));
    assert_eq!(realised.amount_in.is_some(), matches!(realised.outcome, Outcome::Success));
    assert!(realised.amount_in.map_or(true, |taken| taken <= fixture.route.amount_in));
    assert_eq!(realised.tx_ref.is_some(), sends == Sends::Transactions);
    if realised.provenance == Provenance::Landed {
        assert_eq!(sends, Sends::Transactions);
    }
}
```

`sends` is what the executor under test does with a command, and the caller states it: `Sends::Nothing`
for a stub, a throwaway run (`EvmSimulated`, `JupiterSimulated`) or a consumer's paper model;
`Sends::Transactions` for an adapter over a sender, signing or fork. A report cannot say which by
itself, because a fork send and an `eth_call` are both `Simulated`. `liquidity_executor_contract` and
`assert_liquidity_shape` take it too.

The suites, all in `src/testkit/contract.rs`:

| suite | runs against | asserts |
|---|---|---|
| `dex_executor_contract` | every `DexExecutor`: `DexStub`, `EvmSimulated`, `EvmLive` on anvil, `JupiterSimulated`, and `JupiterLive` on Surfpool | the two shape rules above |
| `cex_executor_contract` | every `CexExecutor`, with `reduce_only: false` | `order_ref.is_some() == Landed` |
| `cex_spot_rejects_reduce_only` | every spot `CexExecutor` | `reduce_only: true` is an error, and nothing reaches the venue |
| `liquidity_executor_contract` | every `LiquidityExecutor`: `LiquidityStub`, `EvmLiquidity` on anvil, `WhirlpoolLiquidity` on Surfpool, and a consumer's paper model | the sequence below |
| `cex_account_contract` | every `CexAccount` | `isolated_margin.is_some() == (margin_mode == Isolated)`; `qty == 0` implies no liquidation price; funding sorted by `ts_ms`, every `ts_ms >= since_ms`, no repeated `venue_ref`; `asset` never empty |
| `evm_balance_reader_contract` | every `EvmBalanceReader` | a read at a named block twice gives the same answer; `Latest` answers |
| `spot_balance_reader_contract` | every `SpotBalanceReader` | no asset empty or repeated; `free` and `locked` never negative |
| `cex_orders_contract` | every `CexOrders` | a fill's shape (`assert_fill_shape`) with a positive quantity, naming the id it was found by when sent; a rejection names its status; `NotFound` and `Open` carry nothing to check |

`liquidity_executor_contract` runs one sequence against every liquidity venue, and asserts the same
things of each:

1. `Open`, then `Add`, then `Remove` of all the liquidity, then `Collect`, then `Close`.
2. **The same event sequence on every venue**: `Opened`, `Added`, `Removed`, `Collected`, `Closed`.
3. **The same accounting on every venue:**
   - With no swap between `Open` and `Remove`, `Removed.released` is what `Opened` and `Added` paid,
     less at most one unit per token **per deposit**: each venue rounds a deposit up and a withdrawal
     down. The sequence makes two deposits and one withdrawal, so the bound is two units per token
     (exact amounts 10.4 and 20.4 pay 11 + 21 = 32, and release 30.8, rounded down to 30). Amended 1
     October: "one unit per token" did not allow for the second deposit's rounding. It is a test's
     tolerance, not a feature.
   - The sum of `transferred` over `Removed` and `Collected` is at least the sum of `released`.
   - `liquidity` is conserved: what `Removed` takes out is what `Opened` and `Added` put in.
4. **The same refusals on every venue**: `Close` on a position that still holds liquidity, and
   `Remove` above the held liquidity, each an `Err` with nothing sent. (`Close` after `Remove` and
   before `Collect` is not tested. On Uniswap's managers the principal is still owed, so it is
   refused. On a Whirlpool that earned no fees, the position is already empty.)
5. **The shape rules on every report**: `event` is `Some` exactly on `Success`, and `tx_ref` is `Some`
   exactly when a transaction was sent. Cost is set whenever something ran.

Run three ways, at different points in the development cycle:

| run against | when | needs |
|---|---|---|
| `Stub` | every test run, unconditionally | nothing |
| `Simulated` (a fork / a sandbox) | before any change to a `Live` adapter ships, and on a recurring schedule even without a code change | fork infrastructure or sandbox credentials. For the liquidity port this is `EvmLiquidity` over anvil and `WhirlpoolLiquidity` over Surfpool (§5b) |
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
    ├── balance/              # §6c: EvmBalanceReader, SpotBalanceReader, EvmBalances, the two stubs
    ├── evm/                  # the EVM chain family's plumbing, shared by every EVM adapter (§5)
    │   ├── mod.rs
    │   ├── rpc.rs            # EvmRpc: JSON-RPC, eth_call (with `from` and state overrides),
    │   │                     # receipts, revert-reason replay, hex/ABI helpers
    │   ├── erc20.rs          # selectors; balance and allowance reads; storage-slot probing
    │   └── tx.rs             # Signer, EvmSender (signing and fork backends), TxOutcome, FeePolicy
    ├── network.rs            # Network (§4)
    ├── solana/               # the Solana family's plumbing (§5): rpc.rs (SolanaRpc),
    │                         # tx.rs (SolanaSender, SolanaTxOutcome), token.rs
    ├── dex/
    │   ├── mod.rs            # DexExecutor, RouteQuote (network), SwapRequest, Prepared (enum),
    │   │                     # EvmCall, SolanaTransaction, Realised (+ cost), Outcome (+ Expired),
    │   │                     # TxCost, EvmCost, SolanaCost, NativeCost
    │   ├── stub.rs           # DexStub (was EvmStub)
    │   ├── evm/
    │   │   ├── live.rs       # EvmLive, over an Arc<EvmSender>
    │   │   └── simulated.rs  # EvmSimulated, over an EvmRpc
    │   └── jupiter/          # JupiterSimulated, JupiterLive (§5)
    ├── liquidity/            # § 5b
    │   ├── mod.rs            # LiquidityCommand, LiquidityEvent, LiquidityReport,
    │   │                     # LiquidityCapabilities, LiquidityExecutor, LandedUnread
    │   ├── stub.rs           # LiquidityStub
    │   ├── uniswap_v3/       # EvmLiquidity: the managers' ABI and logs, over an Arc<EvmSender>
    │   └── whirlpool/        # WhirlpoolLiquidity: instructions, accounts, events
    ├── cex/
    │   ├── mod.rs            # CexExecutor, OrderRequest, OrderStateUnknown, CexFill
    │   ├── account.rs        # § 6b: CexAccount, PerpPosition, MarginState, FundingPayment
    │   ├── orders.rs         # § 6: CexOrders, OrderState
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
