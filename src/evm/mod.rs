//! The EVM chain family's plumbing, shared by every EVM adapter (`SPEC.md`
//! §5, §8): the JSON-RPC client, ERC-20 reads and storage layout, and the
//! one sender per wallet and chain that assigns nonces.

pub mod erc20;
pub mod rpc;
pub mod tx;

pub use rpc::{BlockTag, EvmRpc, Receipt, RpcError, RpcLog};
pub use tx::{EvmSender, FeePolicy, PollSettings, Signer, TxOutcome};

use crate::dex::Prepared;
use alloy_primitives::{keccak256, B256};

/// A key correlating a `Prepared` value with the context an adapter's
/// `prepare()` stashed for it, so `execute()` can recover what it needs
/// without the trait signature carrying it.
pub(crate) fn prepared_key(prepared: &Prepared) -> B256 {
    let mut buf = Vec::with_capacity(prepared.to.len() + prepared.calldata.len() + 16);
    buf.extend_from_slice(&prepared.to);
    buf.extend_from_slice(&prepared.calldata);
    buf.extend_from_slice(&prepared.value.to_be_bytes());
    keccak256(buf)
}
