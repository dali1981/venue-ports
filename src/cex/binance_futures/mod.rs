//! Binance USDⓈ-M perpetual futures: `BinanceFuturesLive`, a `CexExecutor`
//! (`SPEC.md` §6, `specs/V2-binance-usdm-futures.md`), and
//! `BinanceFuturesAccount`, a `CexAccount` (`SPEC.md` §6b), over one shared
//! [`BinanceFuturesRest`].
//!
//! Futures speak Binance's usual conventions — the same signing, server
//! clock and error body as spot, shared through `crate::cex::binance` — on
//! another host (`fapi`), with another symbol set, other filters, a
//! position per symbol and reduce-only orders.
//!
//! # Hosts
//!
//! Production is `https://fapi.binance.com`, and is never a default.
//! [`BinanceFuturesConfig::from_env`] defaults to
//! **`https://demo-fapi.binance.com`**, Binance's USDⓈ-M "demo trading"
//! host, which is where the futures testnet (`https://testnet.binancefuture.com`)
//! moved. **Unconfirmed:** this environment cannot reach any Binance host,
//! so which of the two serves the testnet today, and whether it serves
//! every endpoint this module calls, has not been checked. Set
//! `BINANCE_FUTURES_BASE_URL` to the other if the default does not answer.
//! As with spot, pointing the adapter at the testnet *is* its Simulated
//! mode (`SPEC.md` §3): fills there are still `Provenance::Landed`, since a
//! real order reached a sandbox venue.
//!
//! # What the testnet run still has to confirm
//!
//! - **Reduce-only orders are exempt from the notional minimum.**
//!   `execute` skips the `MIN_NOTIONAL` check for them, on the strength of
//!   Binance's own `-4164` message ("Order's notional must be no smaller
//!   than … (unless you choose reduce only)"), so a small remaining
//!   position can always be closed. Not yet seen on the testnet.
//! - **A reduce-only order larger than the position.** Nobody has checked
//!   whether Binance rejects it (`-2022`) or fills up to the position and
//!   expires the rest. `execute` handles both (a plain error, or a partial
//!   `CexFill`), but `CexStub` rejects it until the testnet run
//!   (`BINANCE_FUTURES_RUN_ACCEPTANCE=1`, see `live.rs`) shows which, and
//!   then copies that.
//! - The endpoint versions: `positionRisk` and `account` are read at `v3`,
//!   the rest at `v1`.
//!
//! # Before production
//!
//! Binance restricts USDⓈ-M futures by jurisdiction; the testnet does not
//! check. Confirm the production account is eligible before relying on
//! this.

mod account;
mod filters;
mod live;
mod rest;

pub use account::BinanceFuturesAccount;
pub use live::BinanceFuturesLive;
pub use rest::{BinanceFuturesConfig, BinanceFuturesRest};
