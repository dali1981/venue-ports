//! A tested, reusable connection layer between an automated trading system
//! and the venues it trades on. See `SPEC.md` for the full specification —
//! this crate implements it, module by module, in the order laid out in
//! `IMPLEMENTATION_PLAN.md`.

mod provenance;

pub use provenance::Provenance;

pub mod cex;
pub mod dex;
pub mod evm;
pub mod testkit;
