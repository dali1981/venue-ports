//! Orca's Whirlpool as a liquidity venue (`SPEC.md` §5b, V5 §5):
//! [`WhirlpoolLiquidity`], over a `SolanaSender`.

mod executor;
#[cfg(test)]
mod surfpool_tests;

pub use executor::{WhirlpoolLiquidity, WHIRLPOOL_PROGRAM};
