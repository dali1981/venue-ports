//! `BinanceFuturesRest`: a signed `fapi` client and the venue-clock offset
//! it signs with, shared by `BinanceFuturesLive` and
//! `BinanceFuturesAccount` (through an `Arc`), so both use one set of keys,
//! one connection pool and one clock.
//!
//! Signing, the clock (read at `GET /fapi/v1/time`, refreshed every 10
//! minutes and on `-1021`, with one retry) and the rule for what a failed
//! call means are Binance-wide, in `crate::cex::binance::client`.

use crate::cex::binance::client::BinanceClient;
use crate::cex::CexTimings;
use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde::Deserialize;

/// The default host: Binance's USDⓈ-M futures testnet, now "demo trading".
/// Unconfirmed from here; see the module docs of `binance_futures`.
const TESTNET_BASE_URL: &str = "https://demo-fapi.binance.com";

pub struct BinanceFuturesConfig {
    /// `BINANCE_FUTURES_BASE_URL`; defaults to the testnet host, never
    /// production.
    pub base_url: String,
    /// `BINANCE_FUTURES_API_KEY`, required.
    pub api_key: String,
    /// `BINANCE_FUTURES_API_SECRET`, required.
    pub api_secret: String,
}

impl BinanceFuturesConfig {
    /// Reads `BINANCE_FUTURES_API_KEY` and `BINANCE_FUTURES_API_SECRET`
    /// (required — no defaults for credentials, ever) and
    /// `BINANCE_FUTURES_BASE_URL` (optional, defaults to the **testnet**
    /// host). Futures keys are separate from spot keys. Getting to
    /// production means setting `BINANCE_FUTURES_BASE_URL=https://fapi.binance.com`
    /// explicitly; it is never assumed.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            base_url: std::env::var("BINANCE_FUTURES_BASE_URL")
                .unwrap_or_else(|_| TESTNET_BASE_URL.to_string()),
            api_key: std::env::var("BINANCE_FUTURES_API_KEY")
                .context("BINANCE_FUTURES_API_KEY is not set")?,
            api_secret: std::env::var("BINANCE_FUTURES_API_SECRET")
                .context("BINANCE_FUTURES_API_SECRET is not set")?,
        })
    }
}

#[derive(Deserialize)]
struct PositionMode {
    #[serde(rename = "dualSidePosition")]
    dual_side_position: bool,
}

#[derive(Deserialize)]
struct MultiAssetsMargin {
    #[serde(rename = "multiAssetsMargin")]
    multi_assets_margin: bool,
}

#[derive(Deserialize)]
struct FeeBurn {
    #[serde(rename = "feeBurn")]
    fee_burn: bool,
}

pub struct BinanceFuturesRest {
    client: BinanceClient,
}

impl BinanceFuturesRest {
    pub fn new(config: BinanceFuturesConfig) -> Self {
        Self::with_timings(config, CexTimings::default())
    }

    /// As [`Self::new`], with every wait set by `timings`.
    pub fn with_timings(config: BinanceFuturesConfig, timings: CexTimings) -> Self {
        Self {
            client: BinanceClient::new(
                config.base_url,
                config.api_key,
                config.api_secret,
                "/fapi/v1/time",
                timings,
            ),
        }
    }

    /// The host this client talks to, which says whose account it reads and
    /// trades: the testnet's or production's.
    pub fn base_url(&self) -> &str {
        self.client.base_url()
    }

    pub(crate) fn client(&self) -> &BinanceClient {
        &self.client
    }

    /// This client — connections, keys and clock — with other timings. For
    /// a test that must lose the answer to an order the venue took.
    #[cfg(test)]
    pub(crate) fn with_other_timings(&self, timings: CexTimings) -> Self {
        Self {
            client: self.client.with_timings(timings),
        }
    }

    /// Refuses an account this crate cannot read or trade correctly:
    ///
    /// - a clock whose round trip does not fit inside `recvWindow`;
    /// - hedge mode (`dualSidePosition`), which does not accept
    ///   `reduceOnly` and keeps two positions per symbol;
    /// - multi-assets margin, whose margin is not reported in one asset;
    /// - with `fee_burn_matters`, BNB fee payment (`feeBurn`), which could
    ///   split one order's commission across BNB and the margin asset, and
    ///   `CexFill` holds one asset.
    ///
    /// It reads these settings and never changes them: changing account
    /// configuration is not executing a decided action.
    pub(crate) async fn refuse_unsupported_account(&self, fee_burn_matters: bool) -> Result<()> {
        self.client
            .sync_clock()
            .await
            .context("checking the venue's clock")?;

        let mode: PositionMode = self
            .client
            .signed(Method::GET, "/fapi/v1/positionSide/dual", &[])
            .await
            .map_err(anyhow::Error::from)
            .context("reading the account's position mode")?;
        if mode.dual_side_position {
            bail!(
                "the account is in hedge mode (dualSidePosition): it does not accept reduceOnly \
                 and keeps two positions per symbol. Switch it to one-way mode"
            );
        }

        let margin: MultiAssetsMargin = self
            .client
            .signed(Method::GET, "/fapi/v1/multiAssetsMargin", &[])
            .await
            .map_err(anyhow::Error::from)
            .context("reading the account's margin mode")?;
        if margin.multi_assets_margin {
            bail!(
                "the account is in multi-assets margin mode: its margin is not reported in one \
                 asset. Switch it to single-asset mode"
            );
        }

        if fee_burn_matters {
            let fee: FeeBurn = self
                .client
                .signed(Method::GET, "/fapi/v1/feeBurn", &[])
                .await
                .map_err(anyhow::Error::from)
                .context("reading the account's BNB fee setting")?;
            if fee.fee_burn {
                bail!(
                    "the account pays fees in BNB (feeBurn): one order's commission could then \
                     arrive in BNB and the margin asset, which CexFill cannot hold. Switch BNB \
                     fee payment off"
                );
            }
        }
        Ok(())
    }
}
