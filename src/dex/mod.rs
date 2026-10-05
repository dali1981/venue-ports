//! The DEX port. See `SPEC.md` §5 — this module is the contract; do not
//! diverge from it without updating the spec first.

use crate::{Network, Provenance};
use anyhow::{bail, Result};
use async_trait::async_trait;
use solana_message::{v0, MessageHeader, VersionedMessage};
use solana_transaction::versioned::VersionedTransaction;

pub mod evm;
pub mod jupiter;
mod stub;

pub use stub::{DexStub, RecordedCall};

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

impl PriorityBid {
    /// Refuses any bid above the policy, by name: for an adapter that sends
    /// at its policy only. A bid it ignored would be one the caller counted
    /// as paid and the chain never saw. A bid of nothing is the policy.
    pub fn policy_only(&self, adapter: &str) -> Result<()> {
        match *self {
            PriorityBid::Policy
            | PriorityBid::AbovePolicyPerGas(0)
            | PriorityBid::AbovePolicyLamports(0) => Ok(()),
            bid => bail!("{adapter} sends at its fee policy only, and cannot bid {bid:?} above it"),
        }
    }
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
    /// that cannot send a bid refuses one ([`PriorityBid::policy_only`]),
    /// never ignores it.
    pub priority: PriorityBid,
}

/// A route or command bound to a request: ready to run, one way or
/// another. It passes from a port's `prepare` to its `execute`, and the
/// caller does not read it. An EVM adapter refuses `Prepared::Solana` by
/// name, and the reverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prepared {
    Evm(EvmCall),
    Solana(SolanaTransaction),
}

impl Prepared {
    /// The EVM call, for an EVM adapter; an error naming the other family
    /// otherwise.
    pub fn evm_call(&self) -> Result<&EvmCall> {
        match self {
            Prepared::Evm(call) => Ok(call),
            Prepared::Solana(_) => {
                bail!("a Solana transaction was handed to an EVM adapter: it runs EVM calls only")
            }
        }
    }

    /// What a venue that runs nothing real — a stub, a paper model — hands
    /// back from `prepare`: the network's family's form, distinct for each
    /// `index`, and never sent. On EVM a call to no contract whose calldata
    /// is the index; on Solana an unsigned v0 transaction with `payer` (32
    /// bytes, else the default key) as fee payer, no instructions, and the
    /// index in its blockhash. Its `execute` finds what it prepared by
    /// equality.
    pub fn offline(network: Network, payer: &[u8], index: u64) -> Prepared {
        match network {
            Network::Evm { .. } => Prepared::Evm(EvmCall {
                to: Vec::new(),
                calldata: index.to_be_bytes().to_vec(),
                value: 0,
            }),
            Network::Solana { .. } => {
                let payer = <[u8; 32]>::try_from(payer)
                    .map(solana_address::Address::new_from_array)
                    .unwrap_or_default();
                let mut blockhash = [0u8; 32];
                blockhash[24..].copy_from_slice(&index.to_be_bytes());
                let message = v0::Message {
                    header: MessageHeader {
                        num_required_signatures: 1,
                        num_readonly_signed_accounts: 0,
                        num_readonly_unsigned_accounts: 0,
                    },
                    account_keys: vec![payer],
                    recent_blockhash: solana_hash::Hash::new_from_array(blockhash),
                    instructions: Vec::new(),
                    address_table_lookups: Vec::new(),
                };
                Prepared::Solana(SolanaTransaction {
                    transaction: VersionedTransaction {
                        signatures: Vec::new(),
                        message: VersionedMessage::V0(message),
                    },
                    last_valid_block_height: 0,
                })
            }
        }
    }

    /// The Solana transaction, for a Solana adapter; an error naming the
    /// other family otherwise.
    pub fn solana_transaction(&self) -> Result<&SolanaTransaction> {
        match self {
            Prepared::Solana(tx) => Ok(tx),
            Prepared::Evm(_) => {
                bail!(
                    "an EVM call was handed to a Solana adapter: it runs Solana transactions only"
                )
            }
        }
    }
}

/// A call to one contract: what `Prepared` was before it became an enum.
/// The fields are unchanged, so no EVM encoding changes. What
/// `to`/`calldata`/`value` mean is the adapter's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmCall {
    pub to: ChainAddress,
    pub calldata: Vec<u8>,
    pub value: ChainAmount,
}

/// A transaction built for one fee payer, unsigned until a sender signs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolanaTransaction {
    pub transaction: VersionedTransaction,
    /// The last block height its blockhash is valid at: after it, the
    /// transaction can never land.
    pub last_valid_block_height: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// The swap reverted. There is no output amount; there is a reason.
    Reverted {
        reason: String,
    },
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

/// What a transaction cost, per family (`SPEC.md` §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxCost {
    Evm(EvmCost),
    Solana(SolanaCost),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvmCost {
    /// `None` where a dry run reports none.
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

/// The same three figures for every family, in the chain's smallest native
/// unit (wei, lamports). `deposit` and `spent` are signed: each is the net
/// change in its kind of account, so a sum over any run of transactions is
/// exact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeCost {
    pub fee: u128,
    pub deposit: i128,
    pub spent: i128,
}

impl TxCost {
    /// The same three figures for every family: what a caller records
    /// without branching on the family. On EVM, `deposit` and `spent` are
    /// zero, and `fee` is `gas_used × effective_gas_price + l1_fee`,
    /// counting a missing figure as zero.
    pub fn native(&self) -> NativeCost {
        match self {
            TxCost::Evm(cost) => NativeCost {
                fee: u128::from(cost.gas_used.unwrap_or(0))
                    .saturating_mul(cost.effective_gas_price_wei.unwrap_or(0))
                    .saturating_add(cost.l1_fee_wei.unwrap_or(0)),
                ..NativeCost::default()
            },
            TxCost::Solana(cost) => NativeCost {
                fee: u128::from(cost.fee_lamports),
                deposit: i128::from(cost.rent_deposit_lamports),
                spent: i128::from(cost.rent_spent_lamports),
            },
        }
    }

    /// The cost of nothing, in the family `prepared` belongs to: what an
    /// adapter that runs nothing real (a stub, a paper model) reports.
    pub fn none_for(prepared: &Prepared) -> Self {
        match prepared {
            Prepared::Evm(_) => TxCost::Evm(EvmCost::default()),
            Prepared::Solana(_) => TxCost::Solana(SolanaCost::default()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Realised {
    /// `None` exactly when `outcome` is not `Success`. A revert or a
    /// timeout must never be represented as a zero amount — a zero is a
    /// real, terrible price; "no price" is a different fact.
    pub amount_out: Option<ChainAmount>,
    pub outcome: Outcome,
    /// What it cost, whenever something ran, a revert included. A dry run's
    /// figures are what the run reported: an `eth_call` reports no gas, and
    /// `simulateTransaction` reports `unitsConsumed`.
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
    /// `"evm-simulated"`, `"dex-stub"`.
    fn label(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An adapter that sends at its fee policy takes the policy, and a bid of
    /// nothing, and refuses any bid above it by name.
    #[test]
    fn a_bid_above_the_policy_is_refused_by_an_adapter_that_sends_at_its_policy() {
        for bid in [
            PriorityBid::Policy,
            PriorityBid::AbovePolicyPerGas(0),
            PriorityBid::AbovePolicyLamports(0),
        ] {
            assert!(bid.policy_only("Anyone").is_ok(), "{bid:?}");
        }
        for bid in [
            PriorityBid::AbovePolicyPerGas(1),
            PriorityBid::AbovePolicyLamports(1),
        ] {
            let err = bid.policy_only("Anyone").unwrap_err().to_string();
            assert!(
                err.contains("Anyone") && err.contains("fee policy only"),
                "{err}"
            );
        }
        assert_eq!(PriorityBid::default(), PriorityBid::Policy);
    }

    #[test]
    fn native_cost_is_gas_times_price_plus_the_l1_fee_on_evm() {
        let cost = TxCost::Evm(EvmCost {
            gas_used: Some(150_000),
            effective_gas_price_wei: Some(2_000_000_000),
            l1_fee_wei: Some(7),
        });
        assert_eq!(
            cost.native(),
            NativeCost {
                fee: 300_000_000_000_007,
                ..NativeCost::default()
            }
        );
        assert_eq!(
            TxCost::Evm(EvmCost::default()).native(),
            NativeCost::default()
        );
    }

    #[test]
    fn native_cost_carries_the_signed_rent_on_solana() {
        // A Remove releasing two ticks of a dynamic tick array: their rent
        // moves out of the array and into the position.
        let cost = TxCost::Solana(SolanaCost {
            fee_lamports: 5_000,
            units_consumed: 180_000,
            rent_deposit_lamports: 1_559_040,
            rent_spent_lamports: -1_559_040,
        });
        assert_eq!(
            cost.native(),
            NativeCost {
                fee: 5_000,
                deposit: 1_559_040,
                spent: -1_559_040,
            }
        );
    }

    #[test]
    fn each_family_refuses_the_others_prepared_by_name() {
        let evm = Prepared::Evm(EvmCall {
            to: vec![1; 20],
            calldata: vec![2],
            value: 0,
        });
        assert!(evm.evm_call().is_ok());
        let err = evm.solana_transaction().unwrap_err().to_string();
        assert!(
            err.contains("EVM call") && err.contains("Solana adapter"),
            "{err}"
        );
    }
}
