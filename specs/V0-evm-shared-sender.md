# V0 — one EVM sender per wallet, shared by every EVM adapter

Status: proposed. Needs nothing external. About 1 day.

## Why

`SPEC.md` §5 requires one nonce in flight per chain at a time. `EvmLive` enforces this with
`tx::Signer`'s send lock, which covers the whole sequence of fetching the nonce, signing, broadcasting
and polling for the receipt. But `EvmLive::new(rpc_url, key_hex)` builds its own `Signer`, and the code
that uses the lock (`send_and_confirm`, `ensure_allowance`, fees, gas, nonce, revert reasons) is made
of private methods on `EvmLive`.

V3 adds a second adapter that signs with the same wallet (`EvmLiquidity`). Built the same way, it
would get its own `Signer` from the same key, and with it its own lock. Two locks on one account let two
transactions read the same `pending` nonce. One then replaces the other, or waits behind it. The rule
would hold for each adapter and fail for the wallet.

A second problem follows from the first. After an `Outcome::TimedOut`, the send lock is released while
the transaction may still be in the mempool. The next send reads `eth_getTransactionCount(pending)`. It
then either queues behind the stuck transaction, or, if that transaction was dropped, leaves a nonce gap
that nothing fills. `SPEC.md` says a timed-out transaction "must be resolved out of band before it is
treated as anything else". Today nothing stops a caller from sending on top of it.

`EvmSimulated` and `EvmLive` also each carry their own copy of `rpc_call`, `eth_call`, `pad_address`,
`address_from_slice`, `decode_hex`, `decode_revert_reason` and `decode_abi_string`. V3 would make that
three copies.

## Change

### Module layout (`SPEC.md` §8)

```text
src/
├── evm/                  # the EVM chain family's plumbing, shared by every EVM adapter
│   ├── mod.rs
│   ├── rpc.rs            # EvmRpc: JSON-RPC client, eth_call (with `from` and state overrides),
│   │                     # block number, receipts, revert-reason replay, hex/ABI helpers
│   ├── erc20.rs          # selectors; balance and allowance reads; storage-slot probing
│   │                     # (find_balance_slot, find_allowance_slot, mapping_slot, allowance_slot),
│   │                     # moved out of EvmSimulated
│   └── tx.rs             # Signer (moved from dex/evm/tx.rs) and EvmSender
└── dex/evm/
    ├── live.rs           # EvmLive holds an Arc<EvmSender>
    ├── simulated.rs      # EvmSimulated holds an EvmRpc
    └── stub.rs
```

### `EvmSender`

```rust
/// The only thing in the crate that assigns nonces for one wallet on one
/// chain. Every adapter that sends from that wallet holds the same `Arc`.
pub struct EvmSender { /* EvmRpc, Signer, chain_id, FeePolicy, poll settings, pending: Option<B256> */ }

pub enum TxOutcome {
    Success { block: u64, tx_hash: B256, logs: Vec<RpcLog> },
    Reverted { block: u64, tx_hash: B256, reason: String },
    TimedOut { tx_hash: B256 },
}

/// How max fee and priority fee are chosen. Today's values are tuned for
/// Sepolia: 2 × base fee + priority, and 1.5 gwei when the node has no
/// `eth_maxPriorityFeePerGas`. They become a per-chain setting.
pub struct FeePolicy { pub base_fee_multiplier: u32, pub fallback_priority_fee_wei: u128 }

impl EvmSender {
    /// Checks `eth_chainId` against `chain_id`. Returns an error if this
    /// process already holds an `EvmSender` for the same (address, chain_id).
    pub async fn connect(rpc: EvmRpc, signer: Signer, chain_id: u64, fees: FeePolicy) -> Result<Arc<Self>>;
    pub fn address(&self) -> Address;
    pub fn chain_id(&self) -> u64;

    /// Build, sign, broadcast and poll one transaction to a terminal
    /// outcome, holding the send lock throughout (moved from EvmLive).
    /// Returns an error without sending if an earlier send timed out and has
    /// not been resolved.
    pub async fn send_and_confirm(&self, to: Address, calldata: Vec<u8>, value: U256) -> Result<TxOutcome>;

    /// If `owner`'s allowance to `spender` is below `amount`, approve exactly
    /// `amount`. Never `U256::MAX`. A revert or timeout here is an `Err`, a
    /// setup failure, and not the caller's outcome (moved from EvmLive).
    pub async fn ensure_allowance(&self, token: Address, spender: Address, amount: U256) -> Result<()>;

    /// The hash of a send that ended `TimedOut` and is not yet resolved.
    pub fn unresolved(&self) -> Option<B256>;

    /// Polls the unresolved hash once. Returns its terminal outcome, which
    /// clears it. Returns `None` if it is still pending. Once the node no
    /// longer knows the hash and the account's `latest` nonce has moved past
    /// it, it was replaced or dropped: that is reported as an error naming the
    /// hash, and the hash is cleared.
    pub async fn resolve(&self) -> Result<Option<TxOutcome>>;
}
```

Rules:

- **One `EvmSender` per (address, chain_id) per process.** `connect` enforces this with a process-wide
  registry, and the entry is removed on drop. Nothing can enforce it across processes, so the docs say
  it plainly: one process per wallet per chain.
- **An `EvmSender` belongs to one chain.** `EvmLive::prepare` refuses a `RouteQuote` whose `chain_id`
  is not the sender's. Today the chain id is taken from each route.
- **Nothing is sent over an unresolved timeout.** This makes `Outcome::TimedOut`'s "resolve before
  anything else" rule impossible to skip.

### `EvmLive`

```rust
impl EvmLive {
    pub fn new(sender: Arc<EvmSender>) -> Self;
}
```

`EvmLive::new(rpc_url, key_hex)` goes. Only tests call it. A convenience constructor that builds the
sender is not added, because it would make a second sender for the same wallet the easy thing to write.

### Router return decoding (defect 1 in [README](README.md))

`EvmSimulated` decodes `amount_out` from the **first 32-byte word** of the return data. It returns an
error if there are fewer than 32 bytes, and it never panics. This covers `SwapRouter02.exactInputSingle`
(one word), KyberSwap's `MetaAggregationRouterV2.swap` (`returnAmount, gasUsed`) and the other common
aggregator routers, which all put the output amount first. The module docs state the convention: a
router whose output is not its first word needs its own adapter.

## Tests

- Two adapters sharing one `Arc<EvmSender>` and sending concurrently get consecutive nonces. This runs
  against `wiremock`, recording the nonce on each `eth_sendRawTransaction`.
- A second `connect` for the same (address, chain_id) is an error; a different chain id is not.
- After a forced `TimedOut`, `send_and_confirm` returns an error naming the hash. `resolve` clears the
  hash once a receipt appears.
- `EvmSimulated` given a 64-byte return (`returnAmount, gasUsed`) yields `returnAmount`, and given a
  31-byte return, an error.
- The existing `EvmLive` and `EvmSimulated` tests pass unchanged apart from construction. The
  Sepolia-gated tests still agree to the wei.

## Done when

The above passes, `dex_executor_contract` still passes against all three DEX adapters, and `SPEC.md`
§5 and §8 describe `EvmSender` and the `src/evm/` layout.
