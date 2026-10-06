//! The Uniswap v3 position managers as a liquidity venue (`SPEC.md` §5b):
//! `EvmLiquidity` over an `Arc<EvmSender>`, and the manager ABIs it encodes
//! against. Nothing here crosses the port: the caller sees the commands,
//! events and capabilities of `crate::liquidity`.

pub mod abi;
mod executor;
#[cfg(test)]
mod fork_tests;

pub use executor::{EvmLiquidity, ManagerAbi};
