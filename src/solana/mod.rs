//! The Solana family's plumbing, shared by every Solana adapter (`SPEC.md`
//! §5, the Solana family): the JSON-RPC client, the one sender per wallet and
//! cluster, and the token programs. Transactions, keys and instructions are
//! the Solana SDK's own crates, on one pinned version line.

pub mod rpc;
pub mod token;
pub mod tx;

#[cfg(test)]
mod surfpool_tests;

pub use rpc::{AccountData, Simulation, SolanaRpc, SolanaRpcError, TxMeta};
pub use tx::{SolanaPollSettings, SolanaSender, SolanaTxOutcome};
