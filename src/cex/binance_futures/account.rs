//! `BinanceFuturesAccount`: a `CexAccount` for Binance USDⓈ-M futures
//! (`SPEC.md` §6b, `specs/V4-cex-account-reads.md`). It reads through the
//! same `BinanceFuturesRest` as `BinanceFuturesLive` — keys, connections
//! and clock — held in an `Arc`.
//!
//! # Endpoints, and what is not yet confirmed
//!
//! None of these has been called against the testnet yet: this environment
//! cannot reach Binance. The versions are the ones Binance documents today;
//! "use whichever the testnet serves" (V4) is still to be checked.
//!
//! - **Position:** `GET /fapi/v3/positionRisk?symbol=`. `v3` returns no
//!   margin type or leverage, so those come from
//!   `GET /fapi/v1/symbolConfig?symbol=` whenever the row lacks them (a
//!   `v2`-shaped row carries both and is read as is). `v3` lists only
//!   symbols with a position or open orders, so a flat symbol can come back
//!   as no row at all: it is still an answer, `qty == 0`, with the mark
//!   price and its time from the public `GET /fapi/v1/premiumIndex`, entry
//!   price zero (what Binance reports for a flat row) and, when isolated, an
//!   isolated margin of zero. `isolated_margin` is the row's
//!   `isolatedMargin`; a liquidation price of `0` means none. In one-way
//!   mode, which `connect` requires, there is one row per symbol; more than
//!   one is an error, never a sum.
//! - **Margin:** `GET /fapi/v3/account`: `totalMarginBalance`,
//!   `totalMaintMargin`, `availableBalance`. In single-asset mode, which
//!   `connect` requires, Binance documents these totals as "only for USDT
//!   asset", so `asset` is `"USDT"`. The endpoint carries no time of its
//!   own, so `as_of_ms` is the venue's clock (as this client estimates it)
//!   when the answer arrived.
//! - **Funding:** `GET /fapi/v1/income?incomeType=FUNDING_FEE&symbol=`,
//!   asked in windows of at most 7 days (`startTime`/`endTime`), each paged
//!   by time at 1,000 rows. A full page is followed by another from its
//!   last row's time, de-duplicated by `tranId`; a full page all at one
//!   millisecond cannot be paged past and is an error. This assumes a full
//!   page is the *earliest* 1,000 rows from `startTime`, as Binance
//!   documents for its time-paged endpoints. Binance keeps income for three
//!   months only, so `since_ms` older than 89 days is refused rather than
//!   answered short.

use crate::cex::account::{CexAccount, FundingPayment, MarginMode, MarginState, PerpPosition};
use crate::cex::binance_futures::rest::BinanceFuturesRest;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::Method;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Rows per `income` page: Binance's maximum.
const INCOME_PAGE: usize = 1_000;
/// The widest `startTime`–`endTime` span asked for at once.
const INCOME_WINDOW_MS: i64 = 7 * DAY_MS;
/// How far back income history reaches: "three months", taken as the
/// shortest three months there are.
const INCOME_HISTORY_MS: i64 = 89 * DAY_MS;

#[derive(Debug, Deserialize)]
struct PositionRow {
    symbol: String,
    /// `BOTH` in one-way mode.
    #[serde(rename = "positionSide", default)]
    position_side: Option<String>,
    #[serde(rename = "positionAmt")]
    position_amt: Decimal,
    #[serde(rename = "entryPrice")]
    entry_price: Decimal,
    #[serde(rename = "markPrice")]
    mark_price: Decimal,
    #[serde(rename = "liquidationPrice")]
    liquidation_price: Decimal,
    #[serde(rename = "isolatedMargin", default)]
    isolated_margin: Option<Decimal>,
    /// Present on `v2` rows only.
    #[serde(rename = "marginType", default)]
    margin_type: Option<String>,
    /// Present on `v2` rows only, as a string.
    #[serde(default)]
    leverage: Option<serde_json::Value>,
    #[serde(rename = "updateTime")]
    update_time: i64,
}

#[derive(Debug, Deserialize)]
struct SymbolConfig {
    symbol: String,
    #[serde(rename = "marginType")]
    margin_type: String,
    leverage: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct PremiumIndex {
    #[serde(rename = "markPrice")]
    mark_price: Decimal,
    time: i64,
}

#[derive(Debug, Deserialize)]
struct Account {
    #[serde(rename = "totalMarginBalance")]
    total_margin_balance: Decimal,
    #[serde(rename = "totalMaintMargin")]
    total_maint_margin: Decimal,
    #[serde(rename = "availableBalance")]
    available_balance: Decimal,
}

#[derive(Debug, Deserialize)]
struct IncomeRow {
    symbol: String,
    #[serde(rename = "incomeType")]
    income_type: String,
    income: Decimal,
    asset: String,
    time: i64,
    #[serde(rename = "tranId")]
    tran_id: serde_json::Value,
}

pub struct BinanceFuturesAccount {
    rest: Arc<BinanceFuturesRest>,
}

impl BinanceFuturesAccount {
    /// Checks what these reads rely on — a clock that fits `recvWindow`,
    /// one-way position mode, single-asset margin — then reads through
    /// `rest`. Share `rest` with a `BinanceFuturesLive` to use one set of
    /// keys and one clock for both, or take `BinanceFuturesLive::account`,
    /// whose `connect` already checked all of this.
    pub async fn connect(rest: Arc<BinanceFuturesRest>) -> Result<Self> {
        rest.refuse_unsupported_account(false)
            .await
            .with_context(|| format!("refusing to read the account at {}", rest.base_url()))?;
        Ok(Self { rest })
    }

    /// Over a client whose account `BinanceFuturesLive::connect` checked.
    pub(crate) fn over(rest: Arc<BinanceFuturesRest>) -> Self {
        Self { rest }
    }

    /// The client these reads go through; its base URL says whose account
    /// they read.
    pub fn rest(&self) -> &Arc<BinanceFuturesRest> {
        &self.rest
    }

    async fn symbol_config(&self, symbol: &str) -> Result<(MarginMode, u32)> {
        let rows: Vec<SymbolConfig> = self
            .rest
            .client()
            .signed(
                Method::GET,
                "/fapi/v1/symbolConfig",
                &[("symbol", symbol.to_string())],
            )
            .await
            .with_context(|| format!("reading {symbol}'s margin type and leverage"))?;
        let row = rows
            .into_iter()
            .find(|row| row.symbol == symbol)
            .ok_or_else(|| anyhow!("symbolConfig has no row for {symbol}"))?;
        Ok((margin_mode(&row.margin_type)?, number(&row.leverage)?))
    }

    async fn position_from(&self, symbol: &str, row: PositionRow) -> Result<PerpPosition> {
        if let Some(side) = &row.position_side {
            if side != "BOTH" {
                bail!("{symbol}'s position row is for side {side}: the account is not in one-way mode");
            }
        }
        let (margin_mode, leverage) = match (&row.margin_type, &row.leverage) {
            (Some(margin_type), Some(leverage)) => (margin_mode(margin_type)?, number(leverage)?),
            _ => self.symbol_config(symbol).await?,
        };
        let isolated_margin = match margin_mode {
            MarginMode::Isolated => Some(row.isolated_margin.ok_or_else(|| {
                anyhow!("{symbol} is isolated but its position row has no isolatedMargin")
            })?),
            MarginMode::Cross => None,
        };
        let liquidation_price = if row.position_amt.is_zero() || row.liquidation_price.is_zero() {
            None
        } else {
            Some(row.liquidation_price)
        };
        Ok(PerpPosition {
            symbol: symbol.to_string(),
            qty: row.position_amt,
            entry_price: row.entry_price,
            mark_price: row.mark_price,
            liquidation_price,
            margin_mode,
            leverage,
            isolated_margin,
            as_of_ms: row.update_time,
        })
    }

    /// A symbol `positionRisk` did not list: no position and no open orders.
    async fn flat_position(&self, symbol: &str) -> Result<PerpPosition> {
        let (margin_mode, leverage) = self.symbol_config(symbol).await?;
        let index: PremiumIndex = self
            .rest
            .client()
            .public_get("/fapi/v1/premiumIndex", &[("symbol", symbol.to_string())])
            .await
            .with_context(|| format!("reading {symbol}'s mark price"))?;
        Ok(PerpPosition {
            symbol: symbol.to_string(),
            qty: Decimal::ZERO,
            entry_price: Decimal::ZERO,
            mark_price: index.mark_price,
            liquidation_price: None,
            margin_mode,
            leverage,
            isolated_margin: (margin_mode == MarginMode::Isolated).then_some(Decimal::ZERO),
            as_of_ms: index.time,
        })
    }

    /// Every funding payment for `symbol` from `since_ms` to `until_ms`,
    /// asked in windows of `window_ms` paged by `page` rows. See the module
    /// docs.
    async fn funding_between(
        &self,
        symbol: &str,
        since_ms: i64,
        until_ms: i64,
        window_ms: i64,
        page: usize,
    ) -> Result<Vec<FundingPayment>> {
        let mut payments = BTreeMap::new();
        let mut window_start = since_ms;
        while window_start <= until_ms {
            let window_end = (window_start + window_ms - 1).min(until_ms);
            let mut start = window_start;
            loop {
                let rows: Vec<IncomeRow> = self
                    .rest
                    .client()
                    .signed(
                        Method::GET,
                        "/fapi/v1/income",
                        &[
                            ("symbol", symbol.to_string()),
                            ("incomeType", "FUNDING_FEE".to_string()),
                            ("startTime", start.to_string()),
                            ("endTime", window_end.to_string()),
                            ("limit", page.to_string()),
                        ],
                    )
                    .await
                    .with_context(|| {
                        format!("reading {symbol}'s funding from {start} to {window_end}")
                    })?;
                let full = rows.len() >= page;
                let mut last = start;
                for row in rows {
                    if row.income_type != "FUNDING_FEE" || row.symbol != symbol {
                        bail!(
                            "asked for {symbol}'s FUNDING_FEE income, the venue answered with \
                             {} {} income",
                            row.symbol,
                            row.income_type
                        );
                    }
                    if row.asset.is_empty() {
                        bail!("funding payment {} carries no asset", row.tran_id);
                    }
                    last = last.max(row.time);
                    let venue_ref = number(&row.tran_id)?;
                    payments.insert(
                        venue_ref,
                        FundingPayment {
                            symbol: row.symbol,
                            ts_ms: row.time,
                            amount: row.income,
                            asset: row.asset,
                            venue_ref,
                        },
                    );
                }
                if !full {
                    break;
                }
                if last <= start {
                    bail!(
                        "{page} funding payments for {symbol} share the millisecond {start}: the \
                         venue's paging cannot get past them without skipping some"
                    );
                }
                start = last;
            }
            window_start = window_end + 1;
        }
        let mut payments: Vec<FundingPayment> = payments
            .into_values()
            .filter(|payment| payment.ts_ms >= since_ms)
            .collect();
        payments.sort_by_key(|payment| (payment.ts_ms, payment.venue_ref));
        Ok(payments)
    }
}

#[async_trait]
impl CexAccount for BinanceFuturesAccount {
    async fn position(&self, symbol: &str) -> Result<PerpPosition> {
        let rows: Vec<PositionRow> = self
            .rest
            .client()
            .signed(
                Method::GET,
                "/fapi/v3/positionRisk",
                &[("symbol", symbol.to_string())],
            )
            .await
            .with_context(|| format!("reading {symbol}'s position"))?;
        let mut rows: Vec<PositionRow> = rows.into_iter().filter(|r| r.symbol == symbol).collect();
        match rows.len() {
            0 => self.flat_position(symbol).await,
            1 => self.position_from(symbol, rows.remove(0)).await,
            n => bail!(
                "the venue reported {n} position rows for {symbol}; in one-way mode there is \
                 one, and rows are never summed"
            ),
        }
    }

    async fn margin(&self) -> Result<MarginState> {
        let client = self.rest.client();
        let account: Account = client
            .signed(Method::GET, "/fapi/v3/account", &[])
            .await
            .context("reading the account's margin")?;
        Ok(MarginState {
            asset: "USDT".to_string(),
            margin_balance: account.total_margin_balance,
            maint_margin: account.total_maint_margin,
            available: account.available_balance,
            as_of_ms: client.venue_now_ms(),
        })
    }

    async fn funding_since(&self, symbol: &str, since_ms: i64) -> Result<Vec<FundingPayment>> {
        let now = self.rest.client().venue_now_ms();
        if since_ms < now - INCOME_HISTORY_MS {
            bail!(
                "Binance keeps funding history for three months; since_ms {since_ms} is older \
                 than {} days, so the answer would be cut short",
                INCOME_HISTORY_MS / DAY_MS
            );
        }
        self.funding_between(symbol, since_ms, now, INCOME_WINDOW_MS, INCOME_PAGE)
            .await
    }

    fn label(&self) -> &'static str {
        "binance-futures-account"
    }
}

fn margin_mode(margin_type: &str) -> Result<MarginMode> {
    match margin_type.to_ascii_lowercase().as_str() {
        "isolated" => Ok(MarginMode::Isolated),
        "cross" | "crossed" => Ok(MarginMode::Cross),
        other => bail!("unknown margin type {other:?}"),
    }
}

/// A number the venue sends as a JSON number or a string.
fn number<T: FromStr>(value: &serde_json::Value) -> Result<T> {
    let text = match value {
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(text) => text.clone(),
        other => bail!("expected a number, got {other}"),
    };
    text.parse()
        .map_err(|_| anyhow!("{text:?} is not a number of the expected kind"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::binance::clock::local_now_ms;
    use crate::cex::binance_futures::live::tests::{
        acceptance_opted_in, flatten, mark_price, testnet, testnet_symbol, tradable, tradable_for,
    };
    use crate::cex::binance_futures::rest::BinanceFuturesConfig;
    use crate::cex::binance_futures::BinanceFuturesLive;
    use crate::cex::{CexExecutor, CexTimings, OrderRequest, OrderSide};
    use crate::testkit::contract::{cex_account_contract, CexAccountContractFixture};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    async fn get(server: &MockServer, route: &str, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    /// A one-way, single-asset account.
    async fn venue() -> MockServer {
        let server = MockServer::start().await;
        get(
            &server,
            "/fapi/v1/time",
            serde_json::json!({ "serverTime": local_now_ms() }),
        )
        .await;
        get(
            &server,
            "/fapi/v1/positionSide/dual",
            serde_json::json!({ "dualSidePosition": false }),
        )
        .await;
        get(
            &server,
            "/fapi/v1/multiAssetsMargin",
            serde_json::json!({ "multiAssetsMargin": false }),
        )
        .await;
        server
    }

    async fn account(server: &MockServer) -> BinanceFuturesAccount {
        let rest = Arc::new(BinanceFuturesRest::with_timings(
            BinanceFuturesConfig {
                base_url: server.uri(),
                api_key: "test-key".to_string(),
                api_secret: "test-secret".to_string(),
            },
            CexTimings {
                request_timeout: Duration::from_millis(500),
                ..CexTimings::default()
            },
        ));
        BinanceFuturesAccount::connect(rest).await.unwrap()
    }

    /// A `positionRisk` v3 row: no marginType, no leverage.
    fn v3_row(qty: &str, liquidation: &str, isolated_margin: &str) -> serde_json::Value {
        serde_json::json!({
            "symbol": "BTCUSDT",
            "positionSide": "BOTH",
            "positionAmt": qty,
            "entryPrice": "60000.0",
            "breakEvenPrice": "60024.0",
            "markPrice": "60100.00000000",
            "unRealizedProfit": "-1.00000000",
            "liquidationPrice": liquidation,
            "isolatedMargin": isolated_margin,
            "notional": "-601.00000000",
            "marginAsset": "USDT",
            "isolatedWallet": "0",
            "initialMargin": "120.2",
            "maintMargin": "2.404",
            "positionInitialMargin": "120.2",
            "openOrderInitialMargin": "0",
            "adl": 1,
            "bidNotional": "0",
            "askNotional": "0",
            "updateTime": 1_720_736_417_660i64
        })
    }

    async fn symbol_config(server: &MockServer, margin_type: &str, leverage: u32) {
        get(
            server,
            "/fapi/v1/symbolConfig",
            serde_json::json!([{
                "symbol": "BTCUSDT",
                "marginType": margin_type,
                "isAutoAddMargin": "false",
                "leverage": leverage,
                "maxNotionalValue": "1000000"
            }]),
        )
        .await;
    }

    #[tokio::test]
    async fn reads_a_cross_short_with_margin_type_and_leverage_from_symbol_config() {
        let server = venue().await;
        get(
            &server,
            "/fapi/v3/positionRisk",
            serde_json::json!([v3_row("-0.010", "90000.5", "0")]),
        )
        .await;
        symbol_config(&server, "CROSSED", 5).await;

        let position = account(&server).await.position("BTCUSDT").await.unwrap();

        assert_eq!(
            position,
            PerpPosition {
                symbol: "BTCUSDT".to_string(),
                qty: d("-0.010"),
                entry_price: d("60000.0"),
                mark_price: d("60100"),
                liquidation_price: Some(d("90000.5")),
                margin_mode: MarginMode::Cross,
                leverage: 5,
                isolated_margin: None,
                as_of_ms: 1_720_736_417_660,
            }
        );
    }

    #[tokio::test]
    async fn reads_an_isolated_position_with_its_isolated_margin() {
        let server = venue().await;
        get(
            &server,
            "/fapi/v3/positionRisk",
            serde_json::json!([v3_row("0.010", "30000", "120.5")]),
        )
        .await;
        symbol_config(&server, "ISOLATED", 10).await;

        let position = account(&server).await.position("BTCUSDT").await.unwrap();

        assert_eq!(position.margin_mode, MarginMode::Isolated);
        assert_eq!(position.leverage, 10);
        assert_eq!(position.isolated_margin, Some(d("120.5")));
        assert_eq!(position.qty, d("0.010"));
    }

    #[tokio::test]
    async fn a_row_that_carries_margin_type_and_leverage_needs_no_symbol_config() {
        let server = venue().await;
        let mut row = v3_row("0.010", "30000", "120.5");
        row["marginType"] = serde_json::json!("isolated");
        row["leverage"] = serde_json::json!("20");
        get(&server, "/fapi/v3/positionRisk", serde_json::json!([row])).await;
        Mock::given(path("/fapi/v1/symbolConfig"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let position = account(&server).await.position("BTCUSDT").await.unwrap();

        assert_eq!(position.margin_mode, MarginMode::Isolated);
        assert_eq!(position.leverage, 20);
    }

    #[tokio::test]
    async fn a_flat_symbol_with_no_row_is_an_answer_not_an_error() {
        let server = venue().await;
        get(&server, "/fapi/v3/positionRisk", serde_json::json!([])).await;
        symbol_config(&server, "CROSSED", 20).await;
        get(
            &server,
            "/fapi/v1/premiumIndex",
            serde_json::json!({
                "symbol": "BTCUSDT",
                "markPrice": "60123.4",
                "indexPrice": "60120.0",
                "lastFundingRate": "0.0001",
                "time": 1_720_000_000_123i64
            }),
        )
        .await;

        let position = account(&server).await.position("BTCUSDT").await.unwrap();

        assert!(position.qty.is_zero());
        assert_eq!(position.liquidation_price, None);
        assert_eq!(position.mark_price, d("60123.4"));
        assert_eq!(position.as_of_ms, 1_720_000_000_123);
        assert_eq!(position.margin_mode, MarginMode::Cross);
        assert_eq!(position.isolated_margin, None);
    }

    #[tokio::test]
    async fn a_flat_row_reports_no_liquidation_price() {
        let server = venue().await;
        get(
            &server,
            "/fapi/v3/positionRisk",
            serde_json::json!([v3_row("0", "0", "0")]),
        )
        .await;
        symbol_config(&server, "ISOLATED", 3).await;

        let position = account(&server).await.position("BTCUSDT").await.unwrap();

        assert!(position.qty.is_zero());
        assert_eq!(position.liquidation_price, None);
        assert_eq!(position.isolated_margin, Some(Decimal::ZERO));
    }

    #[tokio::test]
    async fn two_rows_for_one_symbol_are_an_error_never_a_sum() {
        let server = venue().await;
        let mut long = v3_row("0.010", "30000", "0");
        long["positionSide"] = serde_json::json!("LONG");
        let mut short = v3_row("-0.004", "90000", "0");
        short["positionSide"] = serde_json::json!("SHORT");
        get(
            &server,
            "/fapi/v3/positionRisk",
            serde_json::json!([long, short]),
        )
        .await;
        symbol_config(&server, "CROSSED", 5).await;

        let err = account(&server)
            .await
            .position("BTCUSDT")
            .await
            .unwrap_err();

        assert!(err.to_string().contains("2 position rows"), "{err}");
    }

    #[tokio::test]
    async fn reads_margin_from_account_v3() {
        let server = venue().await;
        get(
            &server,
            "/fapi/v3/account",
            serde_json::json!({
                "totalInitialMargin": "120.2",
                "totalMaintMargin": "2.404",
                "totalWalletBalance": "15000.0",
                "totalUnrealizedProfit": "-1.0",
                "totalMarginBalance": "14999.0",
                "totalPositionInitialMargin": "120.2",
                "totalOpenOrderInitialMargin": "0",
                "totalCrossWalletBalance": "15000.0",
                "totalCrossUnPnl": "-1.0",
                "availableBalance": "14878.8",
                "maxWithdrawAmount": "14878.8",
                "assets": [],
                "positions": []
            }),
        )
        .await;

        let before = local_now_ms();
        let margin = account(&server).await.margin().await.unwrap();

        assert_eq!(margin.asset, "USDT");
        assert_eq!(margin.margin_balance, d("14999.0"));
        assert_eq!(margin.maint_margin, d("2.404"));
        assert_eq!(margin.available, d("14878.8"));
        assert!((margin.as_of_ms - before).abs() < 5_000);
    }

    /// An `income` endpoint over `rows` (`(time, tranId)`), answering like
    /// Binance: rows in `[startTime, endTime]`, oldest first, at most
    /// `limit`.
    async fn income(server: &MockServer, rows: Vec<(i64, u64)>) {
        Mock::given(method("GET"))
            .and(path("/fapi/v1/income"))
            .respond_with(move |request: &Request| {
                let param = |key: &str| -> i64 {
                    request
                        .url
                        .query_pairs()
                        .find(|(k, _)| k == key)
                        .unwrap()
                        .1
                        .parse()
                        .unwrap()
                };
                let (start, end, limit) = (param("startTime"), param("endTime"), param("limit"));
                let page: Vec<serde_json::Value> = rows
                    .iter()
                    .filter(|(time, _)| (start..=end).contains(time))
                    .take(limit as usize)
                    .map(|(time, tran_id)| {
                        serde_json::json!({
                            "symbol": "BTCUSDT",
                            "incomeType": "FUNDING_FEE",
                            "income": "-0.01250000",
                            "asset": "USDT",
                            "info": "FUNDING_FEE",
                            "time": time,
                            "tranId": tran_id,
                            "tradeId": ""
                        })
                    })
                    .collect();
                ResponseTemplate::new(200).set_body_json(page)
            })
            .mount(server)
            .await;
    }

    async fn income_calls(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/fapi/v1/income")
            .count()
    }

    #[tokio::test]
    async fn funding_is_paged_through_three_pages_without_losing_or_repeating_a_row() {
        let server = venue().await;
        let since = local_now_ms() - 2 * DAY_MS;
        // 2 500 payments a minute apart, inside one window.
        let rows: Vec<(i64, u64)> = (0..2_500)
            .map(|i| (since + 60_000 * (i + 1), 10_000 + i as u64))
            .collect();
        income(&server, rows).await;

        let funding = account(&server)
            .await
            .funding_since("BTCUSDT", since)
            .await
            .unwrap();

        assert_eq!(funding.len(), 2_500);
        assert_eq!(income_calls(&server).await, 3);
        assert!(funding.windows(2).all(|pair| pair[0].ts_ms < pair[1].ts_ms));
        assert_eq!(funding[0].venue_ref, 10_000);
        assert_eq!(funding[2_499].venue_ref, 12_499);
        assert_eq!(funding[0].amount, d("-0.0125"));
        assert_eq!(funding[0].asset, "USDT");
    }

    #[tokio::test]
    async fn funding_is_asked_in_windows_of_at_most_seven_days() {
        let server = venue().await;
        let since = local_now_ms() - 20 * DAY_MS;
        // Every 8 hours for 20 days.
        let rows: Vec<(i64, u64)> = (0..60)
            .map(|i| (since + 8 * 60 * 60 * 1000 * i, i as u64))
            .collect();
        income(&server, rows).await;

        let funding = account(&server)
            .await
            .funding_since("BTCUSDT", since)
            .await
            .unwrap();

        assert_eq!(funding.len(), 60);
        assert_eq!(income_calls(&server).await, 3);
        for request in server.received_requests().await.unwrap() {
            if request.url.path() != "/fapi/v1/income" {
                continue;
            }
            let param = |key: &str| -> i64 {
                request
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == key)
                    .unwrap()
                    .1
                    .parse()
                    .unwrap()
            };
            assert!(param("endTime") - param("startTime") < INCOME_WINDOW_MS);
        }
    }

    #[tokio::test]
    async fn a_full_page_at_one_millisecond_is_an_error_not_a_short_answer() {
        let server = venue().await;
        let at = local_now_ms() - DAY_MS;
        let rows: Vec<(i64, u64)> = (0..1_000).map(|i| (at, i)).collect();
        income(&server, rows).await;

        let err = account(&server)
            .await
            .funding_since("BTCUSDT", at)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("cannot get past them"), "{err}");
    }

    #[tokio::test]
    async fn funding_older_than_the_venue_keeps_is_refused_not_cut_short() {
        let server = venue().await;
        income(&server, Vec::new()).await;

        let err = account(&server)
            .await
            .funding_since("BTCUSDT", local_now_ms() - 100 * DAY_MS)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("three months"), "{err}");
        assert_eq!(income_calls(&server).await, 0);
    }

    #[tokio::test]
    async fn connect_refuses_hedge_mode() {
        let server = MockServer::start().await;
        get(
            &server,
            "/fapi/v1/time",
            serde_json::json!({ "serverTime": local_now_ms() }),
        )
        .await;
        get(
            &server,
            "/fapi/v1/positionSide/dual",
            serde_json::json!({ "dualSidePosition": true }),
        )
        .await;
        let rest = Arc::new(BinanceFuturesRest::new(BinanceFuturesConfig {
            base_url: server.uri(),
            api_key: "k".to_string(),
            api_secret: "s".to_string(),
        }));

        let err = match BinanceFuturesAccount::connect(rest).await {
            Ok(_) => panic!("connect accepted a hedge-mode account"),
            Err(err) => format!("{err:#}"),
        };
        assert!(err.contains("hedge mode"), "{err}");
    }

    #[tokio::test]
    async fn satisfies_the_account_contract_against_a_mocked_venue() {
        let server = venue().await;
        get(
            &server,
            "/fapi/v3/positionRisk",
            serde_json::json!([v3_row("-0.010", "90000", "0")]),
        )
        .await;
        symbol_config(&server, "CROSSED", 5).await;
        get(
            &server,
            "/fapi/v3/account",
            serde_json::json!({
                "totalMarginBalance": "100", "totalMaintMargin": "1", "availableBalance": "90"
            }),
        )
        .await;
        let since = local_now_ms() - DAY_MS;
        income(&server, vec![(since + 1, 1), (since + 2, 2)]).await;

        cex_account_contract(
            &account(&server).await,
            CexAccountContractFixture {
                symbol: "BTCUSDT".to_string(),
                since_ms: since,
            },
        )
        .await;
    }

    // ---- Gated runs against the real testnet --------------------------
    //
    // Same gates as `live.rs`: BINANCE_FUTURES_API_KEY/SECRET for reads,
    // plus BINANCE_FUTURES_RUN_ACCEPTANCE=1 for the one that trades.

    #[tokio::test]
    async fn testnet_satisfies_the_account_contract() {
        let Some(config) = testnet() else {
            eprintln!("skipping: BINANCE_FUTURES_API_KEY / BINANCE_FUTURES_API_SECRET not set");
            return;
        };
        let account = BinanceFuturesAccount::connect(Arc::new(BinanceFuturesRest::new(config)))
            .await
            .unwrap_or_else(|err| panic!("connecting to the testnet: {err:#}"));
        let since_ms = account.rest().client().venue_now_ms() - 30 * DAY_MS;
        cex_account_contract(
            &account,
            CexAccountContractFixture {
                symbol: testnet_symbol(),
                since_ms,
            },
        )
        .await;
    }

    /// V4's testnet check: a short opened with `BinanceFuturesLive` reads
    /// back here with the fill's quantity and entry price, and reads flat
    /// after a reduce-only close.
    #[tokio::test]
    async fn testnet_reads_back_a_short_opened_and_closed_by_the_executor() {
        let Some(config) = testnet() else {
            eprintln!("skipping: BINANCE_FUTURES_API_KEY / BINANCE_FUTURES_API_SECRET not set");
            return;
        };
        if !acceptance_opted_in() {
            eprintln!("skipping: set BINANCE_FUTURES_RUN_ACCEPTANCE=1 to open and close a short");
            return;
        }
        let symbol = testnet_symbol();
        let live = BinanceFuturesLive::connect(config, &[&symbol])
            .await
            .unwrap_or_else(|err| panic!("connecting to the testnet: {err:#}"));
        let account = live.account();
        assert!(
            account.position(&symbol).await.unwrap().qty.is_zero(),
            "refusing to trade on top of an existing {symbol} position on the testnet"
        );
        let price = mark_price(&live, &symbol).await;
        let quantity = tradable(&tradable_for(&live, &symbol), price, 0);

        let opened = live
            .execute(&OrderRequest {
                symbol: symbol.clone(),
                side: OrderSide::Sell,
                quantity,
                quoted_price: price,
                reduce_only: false,
            })
            .await
            .unwrap();
        let short = account.position(&symbol).await.unwrap();
        assert_eq!(short.qty, -opened.filled_qty);
        assert!(
            (short.entry_price - opened.filled_price).abs() <= d("0.01"),
            "entry {} vs fill {}",
            short.entry_price,
            opened.filled_price
        );

        live.execute(&OrderRequest {
            symbol: symbol.clone(),
            side: OrderSide::Buy,
            quantity: opened.filled_qty,
            quoted_price: price,
            reduce_only: true,
        })
        .await
        .unwrap();
        let closed = account.position(&symbol).await.unwrap();
        assert!(closed.qty.is_zero(), "still {} after closing", closed.qty);
        assert_eq!(closed.liquidation_price, None);
        flatten(&live, &symbol, price).await;
    }
}
