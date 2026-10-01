# V5: one contract for every venue, and the Solana family

**Status: accepted 1 October 2026, with §12's recommendations (Mo: "go ahead with recommendations"). Nothing built.** Written for the network
plan's W6.4 (arb-searcher `feature/solana`). Once accepted, its signatures are copied into `SPEC.md` (§3,
§4, §5, §5b, §7, §8) and its phase is added to `IMPLEMENTATION_PLAN.md`, before any code.

Decided so far (Mo, 1 October):

- `Prepared` becomes an enum, with an EVM form and a Solana form.
- The family is named **Solana**, never "Svm".
- Signing and transactions use the Solana SDK crates.
- A transaction's cost is per family: `Evm(EvmCost)` and `Solana(SolanaCost)`.
- **The venue abstraction is pm-trading's Point 9** ("Fix Venue Abstraction & Enforce Cross-Venue
  Parity"). Do not make Solana fit Uniswap, and do not hand the caller a venue's own action types (the
  first draft's `WhirlpoolAction` did).

## 1. The rule: one strict contract, venues are adapters

Point 9 asks of every venue:

1. **One command interface.** Every action goes through it. The caller never calls a venue directly.
2. **One event interface.** Normalized and deterministic, enough to rebuild the position's state, with
   no venue-specific event.
3. **Explicit capabilities.** The caller branches only on them, never on the venue's name.
4. **The simulator is a venue** like any other, not a special case.
5. **One contract suite.** The same sequence runs against every venue, and asserts the same events
   and the same state changes.

In this crate:

- **A port is the contract**: the DEX port (§5) and the liquidity port (§5b). Its commands, events and
  capabilities are the only types a caller sees.
- **A venue is an adapter behind the port.** Uniswap v3's position manager, Orca's Whirlpool and Jupiter
  are venues. A venue's own types (its ABI, its instructions, its accounts, its program's events) live
  inside its adapter and never cross the port.
- **The paper simulator is a venue too.** arb-searcher's `LiquidityPaper` and `DexPaper` implement the
  same ports and pass the same contract suite. They are adapters, not modes (§2).

## 2. Modes: Live, Simulated, Stub. "Fork" is not a mode

The first draft's "Fork" column was wrong. `SPEC.md` §3 has three modes, and this spec keeps them:

| Mode | What it is | Provenance |
| --- | --- | --- |
| **Live** | A real transaction, signed and sent to the real chain | `Landed` |
| **Simulated** | The real protocol runs, and nothing real is spent. Two ways, chosen by what the port needs (below) | `Simulated` |
| **Stub** | No network. Each outcome is programmed by the test | `Simulated` |

**Simulated runs one of two ways:**

- **Dry run.** One call against the real chain's current state, and the result is thrown away:
  `eth_call` (`EvmSimulated`), `simulateTransaction` (`JupiterSimulated`). This is enough for a swap,
  which is one action.
- **On a fork.** The Live adapter, unchanged, sends to a throwaway copy of the chain: anvil on EVM,
  Surfpool on Solana. The sender's fork backend makes it Simulated (§5b's `EvmSender::fork`). This is
  what a liquidity position needs. Its actions build on each other (open, then remove, then collect),
  and a dry run forgets each one's state before the next.

So "fork" names how Simulated runs a sequence, the way `SPEC.md` §5b already runs `EvmLiquidity` on
anvil. It is not a fourth mode.

The paper simulator is something else: our own model of a venue, with no protocol and no chain. Point 9
treats it as a venue (§1), and its outcomes are `Simulated`.

## 3. Shared types that change

### 3.1 `Prepared` becomes an enum (decided)

```rust
pub enum Prepared {
    Evm(EvmCall),
    Solana(SolanaTransaction),
}

/// Today's `Prepared`, renamed: a call to one contract.
pub struct EvmCall { pub to: ChainAddress, pub calldata: Vec<u8>, pub value: ChainAmount }

/// A transaction built for one fee payer, unsigned until a sender signs it.
pub struct SolanaTransaction {
    pub transaction: solana_transaction::versioned::VersionedTransaction,
    /// The last block height its blockhash is valid at: after it, the transaction can never land.
    pub last_valid_block_height: u64,
}
```

`Prepared` passes from a port's `prepare` to its `execute`, and the caller does not read it. An EVM
adapter refuses `Prepared::Solana` by name, and the reverse. `EvmCall`'s fields are today's, so no EVM
encoding changes.

### 3.2 A command names its network, not an EIP-155 id

`RouteQuote.chain_id: u64` (and the liquidity types' `chain_id`) mean nothing on Solana. They become:

```rust
pub enum Network {
    Evm { chain_id: u64 },
    /// Identified by its genesis hash, which the adapter checks against its node at construction.
    Solana { genesis_hash: [u8; 32] },
}
```

`ChainAddress` stays bytes: 20 on EVM, 32 on Solana. The adapter checks the length.

### 3.3 `Outcome::Expired`: known never to have landed

`TimedOut` means the money's state is unknown. Solana can answer that question. Once the blockhash has
expired (`last_valid_block_height` is past) and the signature has no status, the transaction can never
land.

```rust
pub enum Outcome { Success, Reverted { reason: String }, TimedOut, Expired }
```

No EVM adapter returns it.

### 3.4 What a transaction cost: per family (decided), read the same way

```rust
pub enum TxCost {
    Evm(EvmCost),
    Solana(SolanaCost),
}
pub struct EvmCost {
    pub gas_used: Option<u64>,                 // None where a dry run reports none
    pub effective_gas_price_wei: Option<u128>,
    pub l1_fee_wei: Option<u128>,              // a rollup's data fee, from the receipt
}
pub struct SolanaCost {
    pub fee_lamports: u64,             // signature fee + priority fee, from the transaction's meta
    pub units_consumed: u64,
    pub rent_deposited_lamports: u64,  // paid into accounts that return it when closed (a position)
    pub rent_spent_lamports: u64,      // paid into accounts that never return it (a tick array)
    pub rent_returned_lamports: u64,   // given back by an account this transaction closed
}

impl TxCost {
    /// The same four figures for every family, in the chain's smallest native unit (wei,
    /// lamports): what a caller records without branching on the family.
    pub fn native(&self) -> NativeCost;
}
pub struct NativeCost { pub fee: u128, pub deposited: u128, pub spent: u128, pub returned: u128 }
```

Both ports' outcomes carry `cost: TxCost` whenever something ran, a revert included. On EVM,
`deposited`, `spent` and `returned` are zero.

## 4. The liquidity port: commands, events, capabilities

### 4.1 Commands

```rust
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

pub struct Range { pub network: Network, pub pool: ChainAddress, pub tick_lower: i32, pub tick_upper: i32 }
pub struct Deposit { pub max: TokenPair, pub guard: DepositGuard }
/// The slippage guard. A venue enforces one kind, and says which in its capabilities.
pub enum DepositGuard {
    MinAmounts(TokenPair),
    SqrtPriceBand { min_sqrt_price_x64: u128, max_sqrt_price_x64: u128 },
}
/// Opaque to the caller: an ERC-721 id under a manager, a position mint under a program.
pub struct PositionId { pub network: Network, pub bytes: Vec<u8> }
pub struct TokenPair { pub token0: u128, pub token1: u128 }   // in the pool's own token order

pub struct LiquidityRequest { pub owner: ChainAddress, pub deadline_unix_secs: u64 }
```

What leaves the command, against today's types:

- `PoolKey`, `manager` and the token addresses. The adapter knows its venue's deployment (a manager
  address, a program id) from its construction. It reads the pool's tokens and key from the pool.
- `Collect`'s caps. The one caller always collects everything.
- **Liquidity as an input.** Both venues take a deposit in token amounts, and the venue computes the
  liquidity. Uniswap's manager does this in `mint` and `increaseLiquidity`. Whirlpool does it in
  `increase_liquidity_by_token_amounts_v2`, which takes `token_max_a`, `token_max_b` and a sqrt-price
  band. So the crate still never computes liquidity from prices (`SPEC.md` §5b).

### 4.2 Events

```rust
pub enum LiquidityEvent {
    Opened { position: PositionId, liquidity: u128, paid: TokenPair },
    Added { liquidity: u128, paid: TokenPair },
    /// `released`: the principal taken out of the range. `transferred`: what reached the owner
    /// in this transaction.
    Removed { liquidity: u128, released: TokenPair, transferred: TokenPair },
    Collected { transferred: TokenPair },
    Closed,
}

/// One per command executed: today's `LiquidityRealised`, normalized.
pub struct LiquidityReport {
    pub outcome: Outcome,
    pub event: Option<LiquidityEvent>,   // Some exactly when outcome is Success
    pub cost: TxCost,                    // whenever something ran
    pub at: u64,                         // block or slot
    pub provenance: Provenance,
    pub tx_ref: Option<Vec<u8>>,         // Some exactly when Landed
}
```

The events state facts, never a venue's semantics. A position's token flow is the sum of `paid`, less
the sum of `transferred`. The fees it earned are the sum of `transferred`, less the sum of `released`.
The caller computes both the same way for every venue.

### 4.3 Capabilities

```rust
pub struct LiquidityCapabilities {
    /// The guard the venue enforces on a deposit.
    pub deposit_guard: DepositGuardKind,          // MinAmounts | SqrtPriceBand
    /// A Remove transfers the principal at once. Otherwise it stays owed until Collect.
    pub remove_transfers: bool,
    /// Opening a range may create accounts whose rent never returns (`NativeCost.spent`).
    pub open_may_spend_rent: bool,
    /// Close returns a deposit (`NativeCost.returned`).
    pub close_returns_deposit: bool,
}

#[async_trait]
pub trait LiquidityExecutor: Send + Sync {
    fn capabilities(&self) -> LiquidityCapabilities;
    async fn prepare(&self, cmd: &LiquidityCommand, req: &LiquidityRequest) -> Result<Prepared>;
    async fn execute(&self, prepared: &Prepared) -> Result<LiquidityReport>;
    fn label(&self) -> &'static str;
}
```

The caller branches on `deposit_guard` alone, to build the guard. The other three explain the events'
and costs' figures, and no accounting needs them.

### 4.4 The rules every venue keeps

Point 9's phase 3, applied here. Each rule holds on every venue, and the contract suite checks it:

- **A command the position cannot take is refused before sending**, an `Err` with nothing sent. Removing
  more liquidity than the position holds, closing a position that holds liquidity or owes tokens, and
  acting on a position the owner does not hold are refused this way. Every venue can read the position
  first.
- **Collect on a position that owes nothing** is a success that transfers zero.
- **A passed deadline** is refused without sending. On Solana, the blockhash also bounds the
  transaction once sent (`Expired`).
- **A transaction that ran and whose effect cannot be read** is `LandedUnread` (today's rule), on every
  venue.

## 5. The venues

| | Uniswap v3 position managers (EVM) | Orca Whirlpool (Solana) | Paper (arb-searcher) |
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
| Live, Simulated | `EvmLiquidity` over a signing sender, or over anvil (exists) | `WhirlpoolLiquidity` over a signing `SolanaSender` (not built until Mo asks), or over Surfpool | Simulated only |

Inside the Whirlpool adapter, and never outside it: the instructions above, the position mint and its
derived account, the tick arrays (fixed or dynamic), SPL Token and Token-2022 (the `_v2` instructions
take either), and the rent figures it reports in `SolanaCost`.

**The EVM managers move onto the commands with nothing else changed.** `EvmLiquidity`'s encoding, its
checks and its log reading stay the same. Only where its inputs come from changes: the manager address
from construction, the pool's tokens and key read from the pool.

## 6. The DEX port and Jupiter

The DEX port is already one normalized command (`RouteQuote` and `SwapRequest`) and one outcome
(`Realised`). Jupiter becomes a venue behind it, with its rules inside the adapter:

- **The payload** is Jupiter's quote response, verbatim (it is what `/swap` takes back).
- **The minimum.** Jupiter's program takes a slippage in bps against its quoted amount. The adapter
  sets it to the smallest value whose on-chain minimum is at or above `SwapRequest.min_amount_out`,
  rounding toward safety. It refuses a minimum above the quoted amount. The literal number stays the
  contract, and the bps are the venue's encoding of it.
- **The amount out** is the destination token account's balance after, less before. It is read from
  `simulateTransaction`'s returned accounts (dry run), or from the landed transaction's post token
  balances.

| Mode | Adapter |
| --- | --- |
| Simulated, dry run | `JupiterSimulated`: `/swap`, then `simulateTransaction` with `sigVerify: false` and `replaceRecentBlockhash: true`, from an account that holds the input token (Solana has no state override). It replaces arb-searcher's `execution/solana.rs` |
| Simulated, on a fork | `JupiterLive` over Surfpool |
| Live | `JupiterLive` over a signing sender: not built until Mo asks |
| Stub | `DexStub` (today's `EvmStub`, renamed: it was never EVM-specific) |

`Realised` gains `cost: TxCost`, which carries the plan's `unitsConsumed`.

## 7. The Solana family: `src/solana/`

| Piece | What it does |
| --- | --- |
| `SolanaRpc` | JSON-RPC over `reqwest`, as `EvmRpc` is: `getGenesisHash`, `getLatestBlockhash`, `getBlockHeight`, `simulateTransaction`, `sendTransaction`, `getSignatureStatuses`, `getTransaction`, `getMultipleAccounts`. It does not use `solana-rpc-client` |
| `SolanaSender` | One per wallet and cluster, shared by every Solana adapter, as `EvmSender` is (V0). It signs, sends, and polls the signature until the outcome is known or the blockhash expires. Two backends: **signing** (a key from the environment, refused until Mo asks for Live) and **fork** (Surfpool, a key generated at run time and never written) |
| The fork check | The fork backend refuses any node that is not Surfpool, as the EVM fork sender refuses one that is not anvil. It asks for a `surfnet_*` method, which a real cluster does not serve |
| SDK crates | `solana-pubkey` (or `solana-address`), `solana-hash`, `solana-instruction`, `solana-message`, `solana-transaction`, `solana-keypair`, `solana-signer`: one version line, pinned |

## 8. Contract suites (§7 of `SPEC.md`)

One sequence runs against every liquidity venue: the stub, `EvmLiquidity` on anvil, `WhirlpoolLiquidity`
on Surfpool, and arb-searcher's paper.

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
4. **The same refusals on every venue**: `Close` on a position that still holds liquidity, and `Remove`
   above the held liquidity, each an `Err` with nothing sent. (`Close` after `Remove` and before
   `Collect` is not tested. On Uniswap's managers the principal is still owed, so it is refused. On a
   Whirlpool that earned no fees, the position is already empty.)
5. **The shape rules on every report**: `event` is `Some` exactly on `Success`, and `tx_ref` is `Some`
   exactly when `Landed`. Cost is set whenever something ran.

The DEX suite stays as it is, and runs against `JupiterSimulated` and `JupiterLive` on Surfpool too.

## 9. Module layout

```text
src/
├── evm/                      # unchanged: EvmRpc, EvmSender, Signer, erc20
├── solana/                   # §7: rpc.rs, tx.rs (SolanaTransaction, SolanaCost, SolanaSender), token.rs
├── dex/
│   ├── mod.rs                # DexExecutor, RouteQuote (network), SwapRequest, Prepared (enum),
│   │                         # Realised (+ cost), Outcome (+ Expired), TxCost, EvmCost, NativeCost
│   ├── stub.rs               # DexStub (was EvmStub)
│   ├── evm/                  # EvmLive, EvmSimulated: unchanged
│   └── jupiter/              # §6: JupiterSimulated, JupiterLive
├── liquidity/
│   ├── mod.rs                # LiquidityCommand, LiquidityEvent, LiquidityReport,
│   │                         # LiquidityCapabilities, LiquidityExecutor, LandedUnread
│   ├── stub.rs               # LiquidityStub
│   ├── uniswap_v3/           # EvmLiquidity: the managers' ABI and logs, moved from liquidity/evm/
│   └── whirlpool/            # WhirlpoolLiquidity: instructions, accounts, events
├── cex/                      # unchanged
└── testkit/contract.rs       # §8
```

## 10. What arb-searcher changes

- **The LP runner** sends `LiquidityCommand`s and reads `LiquidityEvent`s. It branches only on
  `capabilities().deposit_guard`. It builds no venue action and names no venue. Its flow is the same on
  every venue: re-centre is `Remove`, `Collect`, `Close`, the swap, then `Open`.
- **Paper** (`LiquidityPaper`) implements the same contract, with each venue's capabilities taken from
  the venue it models. It passes §8's suite. This is W6.3's paper run on a Whirlpool.
- **Costs to rows.** `TxCost::native()` gives the LP rows `rent_deposit_native` and `rent_native` (W6.3)
  with no family branch.
- **Swaps.** `SwapExecutor::SolanaSimulated` wraps `JupiterSimulated`. Amounts convert at that boundary:
  `U256` in arb-searcher, `u128` in the port, `u64` on Solana. `execution/solana.rs` is deleted.
- **The gates.** `lp_golden`, `lp_fork` (anvil) and the replay goldens pin that no EVM number moves.
  `lp_fork_solana` (T-N24) runs §8's sequence on Surfpool with a swap across the range, and compares the
  amounts with the mirror's prediction.

## 11. Order of work

1. **Dependency spike.** One pinned Solana SDK line builds a v0 transaction, signs it, and round-trips a
   transaction Jupiter built. Settle §12.3.
2. **The shared types** (§3). EVM adapters only rewrap. The whole suite and the anvil tests pass
   unchanged.
3. **The liquidity contract** (§4). `EvmLiquidity` and `LiquidityStub` move onto it, and §8's suite runs
   against both. arb-searcher's runner and paper move onto it at the same time, proven by `lp_golden`
   and `lp_fork`.
4. **`solana/`** (§7), with the Surfpool fork backend and its check.
5. **Jupiter** (§6): `JupiterSimulated`, then `JupiterLive` on Surfpool.
6. **Whirlpool** (§5): `WhirlpoolLiquidity` on Surfpool, through §8's suite.
7. Bump the rev in arb-searcher. Then W6.3's paper Whirlpool, and T-N24.

The branch is `feature/solana` in venue-ports, from `main` once PR #2 (`spec/liquidity-and-perps`) is
merged.

## 12. Decisions (taken 1 October, as recommended)

1. **`Outcome::Expired`** (§3.3), or report an expired blockhash as `TimedOut`. Recommended: `Expired`,
   because Solana knows the transaction cannot land.
2. **`DepositGuard` with two kinds** (§4.1), chosen through a capability. The alternative is one kind
   for every venue, where the adapter converts the other. That conversion computes amounts from prices,
   which the crate does not do. Recommended: two kinds.
3. **Whirlpool's instruction encoding.** Orca's `orca_whirlpools_client` 8.0 builds every instruction,
   on `solana-instruction` 3 and `solana-pubkey` 3. `solana-transaction` 3.0.2 is on `solana-instruction`
   3 too, with its keys from `solana-address` 2 (crates.io, read 1 October). The spike finds out whether
   the two key types are one type. If they are, Orca's client builds the instructions. If not, the
   adapter encodes the six it needs from the program's IDL, and is tested against a transaction Orca's
   SDK built.
4. **Re-centring in place.** Whirlpool has `reposition_liquidity_v2`, which moves a position's range in
   one instruction: Point 9's cancel-replace, for a range. It is left out here, because the runner's
   `Remove`, `Collect`, `Close`, `Open` works on every venue. It would be a `Reposition` command behind a
   capability, when wanted.
5. **Surfpool's cheatcodes** fund the run-time key (SOL and the two tokens), as anvil's set the EVM
   balances. This is to verify once Surfpool is installed (it is not, on this machine).
