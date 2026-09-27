//! Binance Spot — the first `CexLive` venue (`IMPLEMENTATION_PLAN.md` Phase
//! 7, confirmed venue choice). Points at the Spot Testnet
//! (`https://testnet.binance.vision`) by default; a production run needs
//! `BINANCE_BASE_URL=https://api.binance.com` set explicitly, never the
//! other way around.

mod live;
mod rest;

pub use live::BinanceLive;
pub use rest::BinanceConfig;
