//! The liquidity port for EVM position managers (`SPEC.md` §5b):
//! `EvmLiquidity` over an `Arc<EvmSender>`, and the manager ABIs it
//! encodes against.

pub mod abi;
mod executor;
#[cfg(test)]
mod fork_tests;

pub use executor::EvmLiquidity;
