# M8a — venue-ports: the executors can do what React's simulator does today

**Status: proposed, not accepted into `SPEC.md`.** Copied on 6 October 2026 from arb-searcher-rs, the consumer that asked for it
(`refactor_2026-10-05-trading-core/M8-execution-through-venue-ports.md`, version 4, arb-searcher-rs commit `311a358`). Everything
between "The plan, verbatim" and "Appendix" is copied unchanged from that file's §1, §2.1, §2.2 and §6.

## Read this first (written when the plan was copied)

**What this repository can do alone.** `cargo test` here passes on a clean clone of `main` (`52fc9c1`) with nothing from
arb-searcher-rs: 224 passed, 1 ignored. A1, A3 and A4 and the venue-ports half of A2 need nothing else; test them as the existing
tests do, with a wiremock node.

| Task | Here | Not here (arb-searcher-rs, a later step) |
|---|---|---|
| A1 pending block, gas of a dry run | all of it. `BlockTag::Pending` already exists (`src/evm/rpc.rs`) | — |
| A2 `Realised.amount_in` | `Realised` and its five implementations: `EvmSimulated`, `EvmLive`, `JupiterSimulated`, `JupiterLive`, `DexStub`, with their tests | `ChainPaper`, `CurvePaper`, `lp`'s `DexPaper`, trade-probe's `FundedOnFork` implement `DexExecutor`, and `Realised {` appears 26 times in 9 files there (struct literals and signatures together). They are fixed when the pin moves |
| A3 provenance on a `CexExecutor` error | the error side, in the adapters here | the driver (`src/trade_probe/probe.rs`) and `CexPaper` stop passing and start setting it. arb-searcher-rs builds `OrderStateUnknown { symbol, client_order_id, order_ref }` by struct literal in two tests (`src/blotter/sink.rs`, `src/trading/execution/mod.rs`), so a new field on that struct breaks them at the pin move |
| A4 one ERC-20 slot prober with the ERC-7201 namespace | all of it. The namespace and the existing prober are in Appendix A | deleting arb-searcher-rs's two other probers is M8b |
| A5 parity test on a fork | **not doable here.** It compares `PoolSimulated` (in arb-searcher-rs only) with `EvmSimulated`, through that repository's fork bench, `PoolSwapper`'s runtime bytecode, anvil and a Base node | all of it, and exit gates 2, 3 and 4 below |

**Do not** change arb-searcher-rs's `venue-ports` pin or its `[patch]` block, and do not add an exact-output swap (§2.2: Q55, M10).
§6.2 below says the pin moves once, at A1's commit. A2 changes `Realised`, which arb-searcher-rs builds in many places, so the pin
cannot move past A2 without that repository's changes in the same step: when it moves is arb-searcher-rs's to settle, not this
work's. Push each commit here as it is made.
Paths in the copied text that are not labelled "venue-ports" are arb-searcher-rs's and cannot be opened from here; Appendix A
quotes the code M8a's tasks depend on, as of `311a358`. Line numbers in the copied text were read on 6 October and move as soon as A1 lands.

---

*The plan, verbatim: §1, §2.1, §2.2 and §6 of the source, with its own numbering.*

## 1. The words this plan uses

| Word | Meaning |
|---|---|
| **arb trade** | The cross-venue trade: two legs on two venues, one long and one short. "Trade" alone, in this plan, is avoided where it could mean a single leg. (Older documents say "round trip" or "trade" for this; they change when this plan is merged into `MILESTONES.md`.) |
| **leg** | One side of an arb trade: the **chain leg** (a swap) or the **exchange leg** (a market order on Binance). A leg is its order and that order's result |
| **order** | One instruction to one venue for one leg |
| **signal** | What the program saw when it woke: the market readings, the pool's state, what woke it |
| **intent** | The decision to execute an arb trade, with the signal it was made on. Nothing has happened yet |
| **ticket** | An order and everything that came back about it |
| **executor** | venue-ports' port to a venue: `DexExecutor` for a chain, `CexExecutor` for an exchange |
| **dry run** | Running a swap's calldata against a node's state without sending it, to see what it would return (an `eth_call`) |
| **mode** | Where orders go: paper, fork (an anvil copy of a chain), testnet, live (`ExecutionMode`) |

The code's type is `Trade` (`src/trading/trades.rs`), holding two legs' orders. It stays `Trade` unless Mo wants it renamed `ArbTrade`
(about 24 lines under `src/trading` and `src/react`; Q62). The table and dataset names (`trade`, `<pair>.executed`) are wire values and
stay.

## 2. What the review of MILESTONES §M8 found

### 2.1 Two dry-run implementations do one job, and the replacement cannot yet do all of it

`contracts/PoolSwapper.sol` is Solidity: an Ethereum-family (EVM) contract, deployed on Base, BNB Chain and Ethereum. It holds our
inventory and swaps directly in a Uniswap-v3-style or Aerodrome pool, with no router in between. Solana has no contract of ours: it
goes through Jupiter (`JupiterSimulated`, `JupiterLive`).

Three things act on a chain, all behind venue-ports' `DexExecutor`, and all return the same result type, `Realised`:

| | Does | Used in |
|---|---|---|
| `EvmLive` | signs, sends, waits for the receipt | fork, testnet, live: **the executor** |
| `EvmSimulated` (venue-ports) | a dry run: sends nothing, reports what the swap would return | trade-probe and `lp`, in every mode, beside the executor (`src/trade_probe/venues.rs:1-10`) |
| `PoolSimulated` (this repository, older) | the same dry run, written for React and `PoolSwapper` | React, under `--simulate` and for gas measurement |

So `EvmSimulated` is never the executor, in simulation or in live. It is the check made before sending and the re-run made after
landing. It sits in live trading next to `EvmLive`, and `Realised` appears in A2 because it is the result type both return. M8b makes
`EvmSimulated` the only dry run and deletes `PoolSimulated`.

**What this does not change: the edge.** The edge and the quotes come from the pool's own arithmetic (React's mirrors) and the
exchange's touch. The dry run does not estimate them. After a decision finds a candidate, React dry-runs the swap and the answer can
veto it (`swap-short`, `swap-reverted`, `unsimulated`; `src/trading/strategy/refusal.rs:98-100`), and it measures the swap's gas
units (`src/react/run.rs:605`). That check is what could change.

**What `EvmSimulated` lacks that `PoolSimulated` has:**

- the **pending block**. React reads Base's preconfirmed state (Flashblocks), and `PoolSimulated::new(url, pending)` dry-runs on it
  (`src/network.rs:121`). `EvmSimulated` runs at a numbered block (venue-ports `src/dex/evm/simulated.rs:266`). A dry run on an older
  state than the decision priced from vetoes or passes different candidates.
- the **gas of a dry run**. `EvmSimulated` reports none (`:290`, "An eth_call reports no gas"); `PoolSimulated::gas_units` asks the
  node's estimate (`src/execution/pool.rs:304`).

A replay of recorded data cannot show either, so a replay would pass while the live check differed. That is why M8a ends with a parity
test on a fork, and M8b with a control arm (§7.4).

### 2.2 The exact-output swap is not on M8's path

An **exact-input** swap sells exactly X and takes what comes out. An **exact-output** swap receives exactly B and pays what that takes.
The buy leg wants B base tokens, to hedge by selling B on Binance. The decision prices the input Q_B that gives B, and the chain is
then sent an exact-input swap of Q_B. The swap carries a floor, `min_out`, which is 50 bps below the expected output by default
(`DEFAULT_MIN_OUTPUT_BPS`, `src/trading/strategy/basis.rs:27`).

A worked case, AERO at about $0.94, B = 579.5 AERO:

- at the quoted state the swap delivers B plus dust (AERO's first intent got 579.500000894, `M6b-the-decision-on-its-expected-net.md:51`);
- if the pool moves against us before the swap lands, it delivers less, down to the floor: 579.5 × (1 − 0.005) = 576.6 AERO;
- sent **together** with the hedge, the exchange has sold 579.5, so we are short up to 2.9 AERO (about $2.7) until inventory is put right;
- with an exact-output swap we would receive exactly 579.5 and pay up to 0.5 % more quote instead. The loss is the same size (the
  market moved); it shows as a cost, and the position stays flat.

`PoolSwapper` has no exact-output entry point (`contracts/PoolSwapper.sol:112, 191`; a negative amount is refused,
`contracts/test/PoolSwapper.t.sol:405`). Adding one is Solidity, foundry tests, a regenerated runtime (`contracts/export-runtime.sh`),
a venue-ports `SwapRequest` variant, and, live, a redeploy of a contract that holds inventory. Paper cannot see any of it: it prices buys
as exact-output already, in the mirrors (M2b), and its venue fills what was expected. M8's first gate compares two simulators on
exact-input requests. With `ChainFirst` (§8.2) the hedge is sized from what arrived, so the shortfall does not arise. Recommended: M10,
where fork and testnet show it and real inventory is first at stake (Q55).

## 6. M8a — venue-ports

**Goal.** venue-ports can replace `PoolSimulated`.

### 6.1 Tasks

| # | Task | Days |
|---|---|---|
| A1 | `EvmSimulated`: the pending block, and the gas of a dry run (§2.1) | 1 |
| A2 | `Realised` reports the input the swap consumed | 1½ |
| A3 | `CexExecutor` reports its provenance on an error | ½ |
| A4 | One ERC-20 slot prober that knows OpenZeppelin's ERC-7201 namespace | 1 |
| A5 | The parity test: MORPHO/WETH on a Base fork | 1 |

**A1.** A pending-block mode, as `PoolSimulated::new(url, pending)` has. A dry run reports its gas: either `EvmCost.gas_used` is filled
from `eth_estimateGas` with the same overrides as the call (the field exists and is `None` for a dry run), or `EvmSimulated` gets a method
of its own. To settle in A1; the first leaves `DexExecutor` unchanged and gives React's gas measurement one call.

**A2.** `Realised.amount_in: Option<ChainAmount>`, `None` when nothing ran. A V3 swap that reaches its price limit takes less than it was
given (`contracts/PoolSwapper.sol`, `swapV3`'s notice: "`amountInUsed` may be less than `amountIn`"), and Booking must post what the pool
took. `Realised` is the result of every executor, so the change ripples through all nine `DexExecutor` implementations: venue-ports'
`EvmSimulated`, `EvmLive`, `JupiterSimulated`, `JupiterLive` and `DexStub`, and here `ChainPaper`, `CurvePaper`, `lp`'s `DexPaper` and
trade-probe's `FundedOnFork`. Done once, here, it is cheaper. (I have not checked that M8b's parity gate needs it; MILESTONES lists it in M8a.)

**A3.** An `Err` from `CexExecutor::execute` carries which provenance it came from, so the driver stops passing the adapter's
(`self.venues.cex_provenance`, `src/trade_probe/probe.rs`).

**A4.** `src/execution/pool.rs:51-66` has `OZ_ERC20_NAMESPACE` and `candidate_bases`, and probes 24 slots; `src/execution/evm.rs` probes 24
and no namespace; venue-ports' `evm/erc20.rs` probes 16 (`MAX_PROBE_SLOTS`) and no namespace. MORPHO on Base keeps its balances under the
namespace, so venue-ports cannot dry-run it today (`05-execution.md`, "Two ports"). The namespace goes into venue-ports' prober, which
becomes the only one.

**A5.** An ignored fork test beside `src/trade_probe/fork_tests.rs`, because `PoolSimulated` is still here: the same prepared swap through
`PoolSimulated` and `EvmSimulated` (pending mode on and off), both directions, $20 and $2,000, MORPHO/WETH, one pinned block, equal to the
unit, with equal gas units. The 3 October parity test covered AERO/USDC only (`fork_tests.rs:379-405`). It also compares the revert
reasons of `swapV3`'s three errors (`SPL`, `TooLittle`, `TooLate`): venue-ports keeps a custom error's data and leaves decoding to the
caller (`e07110d`), and the decoder, `pool.rs::custom_error`, feeds React's `exec_reason` column.

### 6.2 venue-ports, and the pin

**Done (6 October, at Mo's word):** `m4-priority-bid` pushed to `git@github.com:dali1981/venue-ports.git`, and merged into venue-ports'
`main` by `--no-ff` as `52fc9c1`, which carries `execution-blotter` (21 commits, with `feature/solana`) and `3b99848`. `main`'s tree
equals `3b99848`'s; `cargo test` there: 224 passed, 1 ignored. `main` is pushed.

**Nothing changes in this repository now.** `Cargo.toml` still pins `f663277` and patches venue-ports to the local checkout at
`3b99848`, and that keeps working: the content is identical, so a new pin buys nothing yet. When A1 produces the first new venue-ports
commit, the two `rev =` lines (`Cargo.toml:65`, `crates/evm/Cargo.toml:24`) move once to it, the `[patch]` block (`Cargo.toml:67-71`)
is deleted, and `Cargo.lock` is refreshed. Until then the branch builds only on this machine, which is the state it has been in since M4.
Every M8a commit is pushed as it is made, or the patch comes back.

### 6.3 Exit gate

1. venue-ports: `cargo test` and `cargo fmt --check` pass; each new behaviour has a test.
2. A5's test passes on an anvil fork, as `docs/1.execution/fork-tests.md` describes.
3. This repository builds with no `[patch]` on a clean clone.
4. `scripts/arch/check.sh`, unchanged.

**Numbers.** None. **Live run.** Untouched.


---

## Appendix A — the arb-searcher-rs code M8a refers to

Quoted from arb-searcher-rs at `311a358`, so nothing here needs that repository open.

### A1: what `PoolSimulated` does that `EvmSimulated` does not (`src/execution/pool.rs`)

The pending block is a constructor flag, and `None` for `at` means the pending block when it is set. `gas_units` asks the node for its
estimate of the same call with the same overrides, at `latest` or `pending` and never at a pinned block:

```rust
impl PoolSimulated {
    pub fn new(rpc_url: impl Into<String>, pending: bool) -> Self {
        Self {
            http: reqwest::Client::builder().timeout(Duration::from_secs(20)).build().expect("a client with a timeout builds"),
            rpc_url: rpc_url.into(),
            pending,
            slots: Mutex::new(HashMap::new()),
        }
    }

    fn tag(&self, at: Option<u64>) -> String {
        match at {
            Some(n) => format!("0x{n:x}"),
            None if self.pending => "pending".into(),
            None => "latest".into(),
        }
    }

    /// The gas the swap takes: the node's estimate of the same call, with
    /// the same overrides.
    pub async fn gas_units(&self, p: &Prepared) -> Result<u64> {
        let c = Self::call_of(p)?;
        let (call, overrides) = self.call_and_overrides(c).await?;
        match self.rpc("eth_estimateGas", json!([call, self.tag(None), overrides])).await? {
            Ok(Value::String(v)) => u64::from_str_radix(v.trim_start_matches("0x"), 16).with_context(|| format!("a gas estimate of {v}")),
            Ok(other) => bail!("eth_estimateGas answered {other}"),
            Err(e) => Err(anyhow!("eth_estimateGas refused the swap: {e}")),
        }
    }
```

`EvmSimulated::execute` here reads `self.rpc.block_number()` when `at` is `None` and then runs at `BlockTag::Number(block)`
(`src/dex/evm/simulated.rs:262-266`), so a pending mode has to change that choice and not only add a flag.

### A4: the prober arb-searcher-rs has, and the namespace it knows

`PoolSimulated` probes the swapper contract's balance, and in one `eth_call`: every candidate slot is written with its own sentinel
and the balance the token reads back names the slot. Its candidates are the first 24 slots and the OpenZeppelin v5 namespace. This
crate's `src/evm/erc20.rs` probes `MAX_PROBE_SLOTS` = 16 slots, one `eth_call` each, and no namespace, so MORPHO on Base cannot be
dry-run here today.

```rust
/// OpenZeppelin v5's upgradeable ERC-20 keeps its balances under an ERC-7201
/// namespace (`erc7201:openzeppelin.storage.ERC20`), not in a low slot:
/// MORPHO on Base is one such token.
const OZ_ERC20_NAMESPACE: &str = "52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00";

/// Where a token's balance mapping may be: the first slots, where plain and
/// inherited layouts put it, and the namespaces upgradeable tokens use.
fn candidate_bases() -> Vec<[u8; 32]> {
    let mut bases: Vec<[u8; 32]> = (0..MAX_SLOT).map(|s| word(U256::from(s))).collect();
    bases.push(hex::decode(OZ_ERC20_NAMESPACE).expect("a literal").try_into().expect("32 bytes"));
    bases
}

/// `keccak256(pad(key) ‖ base)`: where `mapping(address => …)` at `base`
/// keeps `key`'s entry.
fn mapping_at(key: Address, base: [u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(key.as_bytes());
    buf[32..].copy_from_slice(&base);
    keccak256(buf)
}

    /// `evm`'s probe for one mapping, in one call: every candidate slot is
    /// written with its own sentinel, `SENTINEL + slot`, and the balance the
    /// token reads back names the slot it keeps balances in. A value that is
    /// none of them means no candidate is the mapping. A node that did not
    /// answer has said nothing about the layout, and that is an error.
    async fn probe_balance_slot(&self, token: Address) -> Result<Option<[u8; 32]>> {
        let swapper: Address = SWAPPER.parse().expect("a literal address");
        let bases = candidate_bases();
        let diff: serde_json::Map<String, Value> = bases
            .iter()
            .enumerate()
            .map(|(i, base)| (hex32(mapping_at(swapper, *base)), json!(hex32(word(U256::from(SENTINEL + i as u64))))))
            .collect();
        let call = json!({"to": format!("{token:?}"), "data": format!("{BALANCE_OF}{}", pad_addr(swapper))});
        let overrides = json!({ format!("{token:?}"): { "stateDiff": diff } });
        match self.rpc("eth_call", json!([call, "latest", overrides])).await? {
            Ok(Value::String(s)) => {
                let read = U256::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or_default();
                let index = read.checked_sub(U256::from(SENTINEL)).filter(|d| *d < U256::from(bases.len()));
                Ok(index.map(|d| bases[d.as_usize()]))
            }
            Ok(other) => bail!("{token:?}'s balanceOf answered {other}"),
            Err(e) => bail!("probing {token:?}'s balance slot: {e}"),
        }
    }

// src/execution/evm.rs
/// How many storage slots to try before giving up on a token's layout.
/// Every ERC20 in circulation puts these in the first few; 24 is generous.
pub(super) const MAX_SLOT: u8 = 24;

/// `balanceOf(address)`
pub(super) const BALANCE_OF: &str = "0x70a08231";

/// An improbable value no token holds by accident, used to recognise the
/// slot that was just written.
pub(super) const SENTINEL: u64 = 12_345_678_901_234_567u64;
```

`balanceOf` of a holder under that namespace is `keccak256(pad(holder) ‖ namespace)`, the same formula as a low slot with the
namespace as `base`. The allowance mapping is the second field of OpenZeppelin's `ERC20Storage` struct, which would put it at the
namespace plus one: that is from the contract's source layout and has not been checked against a chain, so check it against MORPHO's
`allowance` when `find_allowance_slot` learns the namespace.

### A5: where the parity test is

`src/trade_probe/fork_tests.rs` in arb-searcher-rs, `trade_probe_fork_pool_routes_pay_their_quote_to_the_unit` (ignored; it needs
`LP_FORK_*` variables and an anvil fork). The new test goes beside it because `PoolSimulated` is there. It stays there.
