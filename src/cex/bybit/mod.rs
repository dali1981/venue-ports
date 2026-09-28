//! Bybit — the second `CexLive` venue, built alongside Binance from the
//! start rather than after it (`IMPLEMENTATION_PLAN.md` Phase 7). Points at
//! Bybit's testnet (`https://api-testnet.bybit.com`) by default; a
//! production run needs `BYBIT_BASE_URL=https://api.bybit.com` set
//! explicitly.

mod live;
mod rest;

pub use live::BybitLive;
pub use rest::{BybitConfig, BybitRest};
