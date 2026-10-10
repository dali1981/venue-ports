//! Binance Spot — the first `CexLive` venue (`IMPLEMENTATION_PLAN.md` Phase
//! 7, confirmed venue choice). Points at the Spot Testnet
//! (`https://testnet.binance.vision`) by default; a production run needs
//! `BINANCE_BASE_URL=https://api.binance.com` set explicitly, never the
//! other way around.
//!
//! Signing ([`sign`]), the server clock ([`clock`]), the signed HTTP client
//! and its error rule ([`client`]) and order tracking ([`order`]) are shared
//! with Binance USDⓈ-M futures (`crate::cex::binance_futures`), which speaks
//! the same conventions on another host.

mod balances;
pub(crate) mod client;
pub(crate) mod clock;
mod live;
pub(crate) mod order;
mod reads;
mod rest;
pub(crate) mod sign;

pub use live::BinanceLive;
pub use reads::{
    ApiRestrictions, BookTicker, Commission, OrderBookSnapshot, PublicTrade, SymbolCommission,
    SymbolRules,
};
pub use rest::{
    BinanceConfig, BinanceRest, CommissionDiscount, CommissionRates, MakerTaker, OrderCheck,
};
