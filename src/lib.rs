//! A tested, reusable connection layer between an automated trading system
//! and the venues it trades on. See `SPEC.md` for the full specification —
//! this crate implements it, module by module, in the order laid out in
//! `IMPLEMENTATION_PLAN.md`.

mod network;
mod node_url;
mod provenance;

pub use network::Network;
pub use provenance::Provenance;

pub mod balance;
pub mod cex;
pub mod dex;
pub mod evm;
pub mod liquidity;
pub mod solana;
pub mod testkit;

// The production validation tier (`specs/V7-production-validation.md`). The signed clients are
// `pub(crate)`, so the tier is compiled inside this crate: in test builds, where each case is an
// `#[ignore]`d test, and, with the `production` feature, as `production::run`, the one function
// another repository calls to run a case.
#[cfg(any(test, feature = "production"))]
pub mod production;

#[cfg(test)]
mod catalogue;
