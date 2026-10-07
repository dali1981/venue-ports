//! The EVM chain family's plumbing, shared by every EVM adapter (`SPEC.md`
//! §5, §8): the JSON-RPC client, ERC-20 reads and storage layout, and the
//! one sender per wallet and chain that assigns nonces.

pub mod erc20;
pub mod rpc;
pub mod tx;

pub use rpc::{BlockTag, EvmRpc, Receipt, RpcError, RpcLog};
pub use tx::{EvmSender, FeePolicy, PollSettings, Signer, TxOutcome, TxReplacedOrDropped};

use crate::dex::EvmCall;
use alloy_primitives::{keccak256, B256};

/// A key correlating an `EvmCall` with the context an adapter's `prepare()`
/// stashed for it, so `execute()` can recover what it needs without the
/// trait signature carrying it.
pub(crate) fn prepared_key(call: &EvmCall) -> B256 {
    let mut buf = Vec::with_capacity(call.to.len() + call.calldata.len() + 16);
    buf.extend_from_slice(&call.to);
    buf.extend_from_slice(&call.calldata);
    buf.extend_from_slice(&call.value.to_be_bytes());
    keccak256(buf)
}
