# V3 — the liquidity port

Status: proposed. Depends on V0. `LiquidityStub` needs nothing external. The fork mode needs an anvil
fork of the target chain, and the live mode needs a funded testnet key. About 6.5 days, or 9.5 with
Uniswap v4.

## Why

A concentrated-liquidity position goes through a **position manager**, not a router. It is created,
grown, shrunk, harvested and closed. `DexExecutor` cannot express this. `Realised` holds one
`amount_out`, while a mint returns a position id, an amount of liquidity and two token amounts. A
decrease moves nothing until a later collect.

The port is venue connectivity like the other two. It turns a decided action into a known outcome, and
it holds none of the consumer's vocabulary. It gets its own trait because its outcomes have a different
shape, not because it is a different kind of thing.

## Non-goals, in this port's terms

The crate does **not**:

- choose ranges, or compute liquidity from prices or prices from ticks. The manager computes the
  liquidity and the chain reports it, and the crate reads the report back.
- value a position, track what it holds, or remember what it minted. Every call returns a value, and the
  caller keeps what it needs.
- stake positions in gauges or claim emissions. Gauges are other contracts and would need their own spec.
- create pools, or handle native ETH (`value` is always zero, and tokens are ERC-20s). It does not
  support fee-on-transfer tokens: the live adapter detects them (see *Cross-check*) but does not handle
  them.

## Contract (`SPEC.md` §5b)

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

**The shape rules**, which `liquidity_executor_contract` asserts for every implementation:

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

**There is no `at` parameter on `execute`.** For a swap, `at` re-runs an `eth_call` at an older block.
A liquidity action changes state that later actions depend on, so re-running one alone means nothing.
A caller who wants an older state starts the fork at that block.

## Implementations

`SPEC.md` §3 expects a DEX adapter to need three implementations, because a throwaway `eth_call` and
a broadcast are different operations. That does not hold here. A position's life is a sequence of
actions, each depending on the state the last one left, and an `eth_call` discards that state. So
Simulated runs the actions on an **anvil fork**, as real transactions from an impersonated owner. That
is the same operation as Live, sent to a chain that is thrown away afterwards. The result is one
implementation and two senders, which is §3's CEX pattern ("the same implementation, pointed
elsewhere") arrived at by a different route. It also means Simulated and Live share every line of
encoding and decoding, which is exactly what the contract suite is meant to prove.

```text
src/liquidity/
├── mod.rs            # the contract above
├── stub.rs           # LiquidityStub
└── evm/
    ├── abi.rs        # sol! definitions for both manager ABIs and their events
    └── executor.rs   # EvmLiquidity: impl LiquidityExecutor over an Arc<EvmSender>
src/evm/tx.rs         # EvmSender gains its fork backend (below)
```

### `LiquidityStub` (1 day)

It makes no network calls. Each call's outcome is programmed: success with given values,
`Reverted { reason }`, `TimedOut`, an `Err`, or `LandedUnread`. It records every `prepare` and
`execute` it receives. **An action with no programmed outcome is an `Err` naming the action.** A
fallback would have to invent liquidity and amounts, and invented numbers in a test are how a fake
drifts away from the real thing.

### `EvmSender`'s fork backend (1 day, in `src/evm/tx.rs`)

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

    /// Checks that `owner` holds at least `amount` of `token`. A signing
    /// sender returns an error when it does not, so a transaction that would
    /// revert with STF is never sent. A fork sender instead writes the balance
    /// slot with `anvil_setStorageAt` (the slot comes from `evm::erc20`'s
    /// probing), exactly as `EvmSimulated` overrides state today.
    pub async fn ensure_balance(&self, token: Address, amount: U256) -> Result<()>;
}
```

On a fork, `send_and_confirm` calls `anvil_impersonateAccount` and then `eth_sendTransaction` from the
owner. It follows the same receipt polling, revert-reason replay and timeout path as signing, and the
same unresolved-timeout rule (V0).

A side effect: `EvmLive` given a fork sender becomes a swap simulator whose state persists between
calls. A consumer that needs swaps and liquidity actions on one fork (mint, a swap that moves the price,
decrease, collect) gets that without writing a new adapter.

### `EvmLiquidity` (2.5 days: ABI and decoding 1.5, executor 1)

```rust
impl EvmLiquidity {
    pub fn new(sender: Arc<EvmSender>) -> Self;
}
// label(): "evm-liquidity-live" or "evm-liquidity-fork", from the sender
```

**`prepare`:**

- The owner is the sender's address and is not zero. The deadline is set and has not passed. The
  action's `chain_id` is the sender's.
- `Mint`: token0 < token1, tick_lower < tick_upper, the two desired amounts are not both zero, and
  each minimum is at most its desired amount. Tick spacing is not checked here: the manager has the
  authority on that, and a wrong tick gives a revert.
- Any other action: `ownerOf(id)` on the manager must be the owner. For `Increase`, `positions(id)`
  supplies token0 and token1, which the approvals need. `Decrease` requires liquidity above zero (the
  manager requires it too). `Collect` requires at least one cap above zero.
- Encode with `alloy-sol-types` (`sol!`), from the alloy family already in the dependency graph. These
  are struct arguments containing `int24` and `uint24` fields, and they are not hand-encoded.
- Keep the context, keyed by the hash of the `Prepared` value, as `EvmLive` does.

**`execute`:**

1. If the deadline has passed, return an `Err` without sending anything.
2. `Mint` and `Increase`: `ensure_balance` and `ensure_allowance` for each token, with the manager
   as spender and **exactly the desired amount** as the allowance, never `U256::MAX`. Any unspent part
   of an allowance stays behind, capped at that desired amount, and the next action approves exactly
   what it needs.
3. `send_and_confirm(manager, calldata, 0)`.
4. `Success`: decode the events from the manager's own logs (`log.address == manager`). The pool also
   emits `Mint` and `Collect` events with other signatures, and those are ignored. If an expected event
   is missing, return `LandedUnread`.
5. **Cross-check** against the ERC-20 `Transfer` logs in the same receipt. For `Mint` and `Increase`,
   the owner must have sent exactly `amount0` of token0 and `amount1` of token1. For `Collect`, the
   owner must have received exactly those amounts. Amounts of zero are skipped, because the pool
   transfers nothing then. A mismatch gives `LandedUnread` naming both figures. This catches a
   fee-on-transfer token and a wrong decoder.
6. Amounts above `u128` give an error. They are never truncated.
7. `Reverted` and `TimedOut` map onto `Outcome` as they do for swaps.

**Venues:**

| Manager | Chains | ABI | Days |
| --- | --- | --- | --- |
| Uniswap v3 `NonfungiblePositionManager` | Ethereum, Base, BNB Chain, Arbitrum, Optimism, and Sepolia for tests | `PoolKey::Fee` | in the 2.5 above |
| PancakeSwap v3 `NonfungiblePositionManager` | BNB Chain, Base, Ethereum | the same as Uniswap v3; confirm against the verified source | 0 |
| Aerodrome Slipstream `NonfungiblePositionManager` | Base | `PoolKey::TickSpacing`; `mint` takes `tickSpacing` in place of `fee` and ends with `sqrtPriceX96`; confirm the other calls against the verified source | 0.5 |
| Uniswap v4 `PositionManager` | — | `modifyLiquidities(actions, deadline)` with Permit2 approvals. It needs a `PoolKey::V4 { … }` variant and a different event set. Deferred until a consumer needs it | +3 |

Manager addresses are not listed here. The consumer passes them in. The fork fixtures read them from
the environment, and each is checked with `eth_getCode` (non-empty) and `factory()`.

## Contract suite: `liquidity_executor_contract` (1.5 days, with the §9.2 run)

```rust
pub struct LiquidityContractFixture {
    pub range: RangeSpec,
    pub request: LiquidityRequest,
    pub mint: (ChainAmount, ChainAmount),
    pub increase: (ChainAmount, ChainAmount),
}
pub async fn liquidity_executor_contract(executor: &dyn LiquidityExecutor, fixture: LiquidityContractFixture);
```

The suite drives one whole life: mint, increase, decrease all of it, collect everything
(`u128::MAX` caps), burn. At every step it asserts the shape rules. Across the steps it asserts:

- mint gives a position and liquidity above zero, and every later step returns the same position;
- decrease removes exactly the sum of what mint and increase added;
- what collect returns is, per token, at least what decrease credited (the principal plus fees of zero
  or more);
- burn succeeds.

It runs against the stub every time, with a consistent life programmed in, and against a fork sender
under the fork CI job.

**The §9.2 bar, on a fork:** 100 lives with distinct ranges and amounts. Each one is reconciled to the
wei against the chain's own state rather than against the decoded events:

- after mint and increase, `positions(id).liquidity` equals the sum of the deltas;
- the owner's `balanceOf` before and after each step matches `amount0` and `amount1`.

Include one injected failure per `Outcome` variant: a mint whose `amount_min` is above what the range
takes (`Reverted("Price slippage check")`), a burn with liquidity left (`Reverted("Not cleared")`),
and a `TimedOut` forced with `evm_setAutomine(false)`.

**Live** (`SPEC.md` §9.4), only by a person, deliberately: one life on a testnet that has the ABI
(Uniswap v3 on Sepolia or Base Sepolia, PancakeSwap v3 on the BNB Chain testnet). If Slipstream has no
testnet deployment, its first live run is a dust-sized mainnet life, taken the same way.

## Done when

The stub and fork runs pass the suite. The §9.2 fork bar is met for the Uniswap v3 ABI and for
Slipstream's. One live testnet life has been reconciled by a person. `SPEC.md` has §5b, and §8 has the
layout. `examples/` has a lifecycle against `LiquidityStub`.
