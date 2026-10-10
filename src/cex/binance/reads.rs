//! The reads a production run needs beside the order path (`SPEC.md` §6d,
//! `specs/V7-production-validation.md`): the commission an account pays on a
//! symbol, what a key may do, the top of the book and its levels, a symbol's
//! rules, and the trades the venue lists. Each is an inherent method of
//! [`BinanceRest`], as `test_order` is: they are Binance's own, and a second
//! venue would motivate a trait. Shapes are Binance's documented ones, see
//! <https://developers.binance.com/docs/binance-spot-api-docs/rest-api>.
//!
//! Every price, quantity and rate is a [`Decimal`] parsed from the string the
//! venue sent: a JSON number where a string is documented is an error, never
//! a float. A field the venue leaves out is `None`, or an error naming it,
//! never a zero. A read answered with a refusal is an `Err` that carries a
//! [`VenueRefusal`](crate::cex::VenueRefusal); nothing is sent for a request
//! that is refused here first.

use crate::cex::binance::rest::{BinanceRest, CommissionDiscount};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::Method;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer};
use std::str::FromStr;

const ACCOUNT_COMMISSION_PATH: &str = "/api/v3/account/commission";
const API_RESTRICTIONS_PATH: &str = "/sapi/v1/account/apiRestrictions";
const BOOK_TICKER_PATH: &str = "/api/v3/ticker/bookTicker";
const EXCHANGE_INFO_PATH: &str = "/api/v3/exchangeInfo";
const TRADES_PATH: &str = "/api/v3/trades";
const DEPTH_PATH: &str = "/api/v3/depth";

/// The most trades `GET /api/v3/trades` lists.
const MAX_TRADES: u16 = 1_000;
/// The most levels `GET /api/v3/depth` lists on a side.
const MAX_DEPTH: u16 = 5_000;

/// A [`Decimal`] from the string the venue sent. A JSON number is refused: it
/// would arrive through a float.
fn decimal<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Decimal, D::Error> {
    let text = String::deserialize(deserializer)?;
    Decimal::from_str(&text)
        .map_err(|err| serde::de::Error::custom(format!("{text:?} is not a decimal: {err}")))
}

/// Book levels, `[price, quantity]` as two strings, as [`Decimal`]s.
fn levels<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<(Decimal, Decimal)>, D::Error> {
    Vec::<(String, String)>::deserialize(deserializer)?
        .into_iter()
        .map(|(price, quantity)| {
            let parse = |text: &str| {
                Decimal::from_str(text).map_err(|err| {
                    serde::de::Error::custom(format!("{text:?} is not a decimal: {err}"))
                })
            };
            Ok((parse(&price)?, parse(&quantity)?))
        })
        .collect()
}

/// A symbol the venue can be asked about: not empty, which it would refuse
/// after a round trip.
fn check_symbol(symbol: &str) -> Result<()> {
    if symbol.is_empty() {
        bail!("a symbol is required: the venue would refuse an empty one, so nothing was sent");
    }
    Ok(())
}

/// What an account pays on trades of one symbol: each side of a trade's rates,
/// as fractions of the traded amount (`0.001` is 10 bps).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Commission {
    #[serde(deserialize_with = "decimal")]
    pub maker: Decimal,
    #[serde(deserialize_with = "decimal")]
    pub taker: Decimal,
    #[serde(deserialize_with = "decimal")]
    pub buyer: Decimal,
    #[serde(deserialize_with = "decimal")]
    pub seller: Decimal,
}

/// The commission rates `GET /api/v3/account/commission` states for a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SymbolCommission {
    pub symbol: String,
    #[serde(rename = "standardCommission")]
    pub standard: Commission,
    #[serde(rename = "specialCommission")]
    pub special: Commission,
    #[serde(rename = "taxCommission")]
    pub tax: Commission,
    /// The reduction of the standard rate when commission is paid in the
    /// discount asset (BNB).
    pub discount: CommissionDiscount,
}

/// What an API key may do, as `GET /sapi/v1/account/apiRestrictions` says.
/// The venue's other flags are not read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApiRestrictions {
    /// Whether the key works only from the addresses whitelisted for it.
    #[serde(rename = "ipRestrict")]
    pub ip_restrict: bool,
    #[serde(rename = "enableReading")]
    pub enable_reading: bool,
    /// Whether the key may withdraw. A trading key should say `false`.
    #[serde(rename = "enableWithdrawals")]
    pub enable_withdrawals: bool,
    #[serde(rename = "enableInternalTransfer")]
    pub enable_internal_transfer: bool,
    #[serde(rename = "enableSpotAndMarginTrading")]
    pub enable_spot_and_margin_trading: bool,
    #[serde(rename = "enableFutures")]
    pub enable_futures: bool,
    #[serde(rename = "enableMargin")]
    pub enable_margin: bool,
    /// When the key was created, by the venue's clock, in Unix ms.
    #[serde(rename = "createTime", default)]
    pub create_time_ms: Option<u64>,
}

/// The best bid and ask of a symbol (`GET /api/v3/ticker/bookTicker`), as the
/// venue states them. This read does not interpret a zero: a side with nothing
/// resting on it is however the venue says so.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BookTicker {
    pub symbol: String,
    #[serde(rename = "bidPrice", deserialize_with = "decimal")]
    pub bid_price: Decimal,
    #[serde(rename = "bidQty", deserialize_with = "decimal")]
    pub bid_qty: Decimal,
    #[serde(rename = "askPrice", deserialize_with = "decimal")]
    pub ask_price: Decimal,
    #[serde(rename = "askQty", deserialize_with = "decimal")]
    pub ask_qty: Decimal,
}

/// A symbol's trading rules, from `GET /api/v3/exchangeInfo?symbol=`: what an
/// order must satisfy to be accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRules {
    pub symbol: String,
    /// The venue's own status; `"TRADING"` is the one that trades.
    pub status: String,
    pub base_asset: String,
    pub quote_asset: String,
    /// `LOT_SIZE.stepSize`: a quantity is a whole number of these.
    pub lot_step: Decimal,
    /// `LOT_SIZE.minQty`.
    pub min_qty: Decimal,
    /// `NOTIONAL.minNotional`, or `MIN_NOTIONAL.minNotional` where the symbol
    /// has only that one; `None` where it has neither.
    pub min_notional: Option<Decimal>,
    /// Whether `min_notional` applies to a market order
    /// (`NOTIONAL.applyMinToMarket`, `MIN_NOTIONAL.applyToMarket`); `None`
    /// where the venue does not say.
    pub apply_min_to_market: Option<bool>,
}

/// One trade of the venue's public list (`GET /api/v3/trades`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PublicTrade {
    pub id: u64,
    #[serde(deserialize_with = "decimal")]
    pub price: Decimal,
    #[serde(rename = "qty", deserialize_with = "decimal")]
    pub qty: Decimal,
    #[serde(rename = "time")]
    pub time_ms: u64,
    #[serde(rename = "isBuyerMaker")]
    pub is_buyer_maker: bool,
}

/// The levels of a symbol's book (`GET /api/v3/depth`) and the id the venue's
/// own stream numbers its updates by.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OrderBookSnapshot {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: u64,
    /// `(price, quantity)`, best (highest) first.
    #[serde(deserialize_with = "levels")]
    pub bids: Vec<(Decimal, Decimal)>,
    /// `(price, quantity)`, best (lowest) first.
    #[serde(deserialize_with = "levels")]
    pub asks: Vec<(Decimal, Decimal)>,
}

#[derive(Deserialize)]
struct ExchangeInfo {
    symbols: Vec<SymbolInfo>,
}

#[derive(Deserialize)]
struct SymbolInfo {
    symbol: String,
    status: String,
    #[serde(rename = "baseAsset")]
    base_asset: String,
    #[serde(rename = "quoteAsset")]
    quote_asset: String,
    filters: Vec<Filter>,
}

/// The filters this crate reads; any other is skipped.
#[derive(Deserialize)]
#[serde(tag = "filterType")]
enum Filter {
    #[serde(rename = "LOT_SIZE")]
    LotSize {
        #[serde(rename = "minQty", deserialize_with = "decimal")]
        min_qty: Decimal,
        #[serde(rename = "stepSize", deserialize_with = "decimal")]
        step_size: Decimal,
    },
    #[serde(rename = "NOTIONAL")]
    Notional {
        #[serde(rename = "minNotional", deserialize_with = "decimal")]
        min_notional: Decimal,
        #[serde(rename = "applyMinToMarket", default)]
        apply_min_to_market: Option<bool>,
    },
    #[serde(rename = "MIN_NOTIONAL")]
    MinNotional {
        #[serde(rename = "minNotional", deserialize_with = "decimal")]
        min_notional: Decimal,
        #[serde(rename = "applyToMarket", default)]
        apply_to_market: Option<bool>,
    },
    #[serde(other)]
    Other,
}

impl SymbolInfo {
    fn into_rules(self) -> Result<SymbolRules> {
        let symbol = self.symbol;
        let (mut lot, mut notional, mut min_notional_filter) = (None, None, None);
        for filter in self.filters {
            match filter {
                Filter::LotSize { min_qty, step_size } => lot = Some((min_qty, step_size)),
                Filter::Notional {
                    min_notional,
                    apply_min_to_market,
                } => notional = Some((min_notional, apply_min_to_market)),
                Filter::MinNotional {
                    min_notional,
                    apply_to_market,
                } => min_notional_filter = Some((min_notional, apply_to_market)),
                Filter::Other => {}
            }
        }
        let (min_qty, lot_step) =
            lot.ok_or_else(|| anyhow!("exchangeInfo for {symbol} has no LOT_SIZE filter"))?;
        if lot_step.is_zero() {
            bail!(
                "exchangeInfo for {symbol} states a LOT_SIZE stepSize of zero, which means no \
                 step: a quantity could not be rounded to it"
            );
        }
        // `NOTIONAL` is the current filter; `MIN_NOTIONAL` is the one it replaced.
        let (min_notional, apply_min_to_market) = match notional.or(min_notional_filter) {
            Some((min, apply)) => (Some(min), apply),
            None => (None, None),
        };
        Ok(SymbolRules {
            symbol,
            status: self.status,
            base_asset: self.base_asset,
            quote_asset: self.quote_asset,
            lot_step,
            min_qty,
            min_notional,
            apply_min_to_market,
        })
    }
}

/// `levels` are one side of a book, best first: strictly ordered by price.
fn check_book_side(
    symbol: &str,
    side: &str,
    levels: &[(Decimal, Decimal)],
    best_is_highest: bool,
) -> Result<()> {
    let ordered = levels.windows(2).all(|pair| {
        if best_is_highest {
            pair[0].0 > pair[1].0
        } else {
            pair[0].0 < pair[1].0
        }
    });
    if !ordered {
        bail!("the {side} of {symbol}'s book are not in strict price order, best first");
    }
    Ok(())
}

impl BinanceRest {
    /// The commission rates the account pays on `symbol`
    /// (`GET /api/v3/account/commission`, signed, weight 20).
    pub async fn account_commission(&self, symbol: &str) -> Result<SymbolCommission> {
        check_symbol(symbol)?;
        let commission: SymbolCommission = self
            .client()
            .signed(
                Method::GET,
                ACCOUNT_COMMISSION_PATH,
                &[("symbol", symbol.to_string())],
            )
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("reading GET {ACCOUNT_COMMISSION_PATH} for {symbol}"))?;
        if commission.symbol != symbol {
            bail!(
                "asked for {symbol}'s commission, the venue answered for {}",
                commission.symbol
            );
        }
        Ok(commission)
    }

    /// What the key this client signs with may do
    /// (`GET /sapi/v1/account/apiRestrictions`, signed, weight 1). The
    /// production host answers it; the spot testnet has no `/sapi`, and its
    /// answer is an `Err` carrying a refusal.
    pub async fn api_restrictions(&self) -> Result<ApiRestrictions> {
        self.client()
            .signed(Method::GET, API_RESTRICTIONS_PATH, &[])
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("reading GET {API_RESTRICTIONS_PATH}"))
    }

    /// The best bid and ask of `symbol` (`GET /api/v3/ticker/bookTicker`,
    /// public, weight 2).
    pub async fn book_ticker(&self, symbol: &str) -> Result<BookTicker> {
        check_symbol(symbol)?;
        let ticker: BookTicker = self
            .client()
            .public_get(BOOK_TICKER_PATH, &[("symbol", symbol.to_string())])
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("reading GET {BOOK_TICKER_PATH} for {symbol}"))?;
        if ticker.symbol != symbol {
            bail!(
                "asked for {symbol}'s book ticker, the venue answered for {}",
                ticker.symbol
            );
        }
        Ok(ticker)
    }

    /// `symbol`'s trading rules (`GET /api/v3/exchangeInfo?symbol=`, public,
    /// weight 20). A symbol with no `LOT_SIZE` filter, or with a step of zero,
    /// is an `Err`: this crate rounds to the step.
    pub async fn symbol_rules(&self, symbol: &str) -> Result<SymbolRules> {
        check_symbol(symbol)?;
        let info: ExchangeInfo = self
            .client()
            .public_get(EXCHANGE_INFO_PATH, &[("symbol", symbol.to_string())])
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("reading GET {EXCHANGE_INFO_PATH} for {symbol}"))?;
        info.symbols
            .into_iter()
            .find(|row| row.symbol == symbol)
            .ok_or_else(|| anyhow!("exchangeInfo has no row for {symbol}"))?
            .into_rules()
    }

    /// The most recent trades of `symbol`, oldest first
    /// (`GET /api/v3/trades`, public, weight 25). `limit` is 1 to 1000.
    pub async fn recent_trades(&self, symbol: &str, limit: u16) -> Result<Vec<PublicTrade>> {
        check_symbol(symbol)?;
        if limit == 0 || limit > MAX_TRADES {
            bail!("a trade list is 1 to {MAX_TRADES} long, not {limit}: nothing was sent");
        }
        self.client()
            .public_get(
                TRADES_PATH,
                &[("symbol", symbol.to_string()), ("limit", limit.to_string())],
            )
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("reading GET {TRADES_PATH} for {symbol}"))
    }

    /// The levels of `symbol`'s book, `limit` on each side
    /// (`GET /api/v3/depth`, public, weight 5 to 250 by `limit`: 5 up to 100
    /// levels, 25 up to 500, 50 up to 1000, 250 up to 5000). `limit` is 1 to
    /// 5000. Each side is checked to be in strict price order, best first.
    pub async fn order_book(&self, symbol: &str, limit: u16) -> Result<OrderBookSnapshot> {
        check_symbol(symbol)?;
        if limit == 0 || limit > MAX_DEPTH {
            bail!("a book is 1 to {MAX_DEPTH} levels deep, not {limit}: nothing was sent");
        }
        let book: OrderBookSnapshot = self
            .client()
            .public_get(
                DEPTH_PATH,
                &[("symbol", symbol.to_string()), ("limit", limit.to_string())],
            )
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("reading GET {DEPTH_PATH} for {symbol}"))?;
        check_book_side(symbol, "bids", &book.bids, true)?;
        check_book_side(symbol, "asks", &book.asks, false)?;
        Ok(book)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::binance::clock::local_now_ms;
    use crate::cex::binance::rest::BinanceConfig;
    use crate::cex::{refusal_of, CexTimings};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn decimal(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn fast() -> CexTimings {
        CexTimings {
            request_timeout: Duration::from_millis(200),
            recv_window: Duration::from_millis(300),
            clock_refresh: Duration::from_secs(600),
            poll_interval: Duration::from_millis(10),
            poll_timeout: Duration::from_millis(300),
            trades_timeout: Duration::from_millis(200),
        }
    }

    fn rest(server: &MockServer) -> BinanceRest {
        BinanceRest::with_timings(
            BinanceConfig {
                base_url: server.uri(),
                api_key: "test-key".to_string(),
                api_secret: "test-secret".to_string(),
            },
            fast(),
        )
    }

    /// A venue with a clock. Every signed call reads it first.
    async fn venue() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v3/time"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "serverTime": local_now_ms() })),
            )
            .mount(&server)
            .await;
        server
    }

    async fn answering(server: &MockServer, route: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn json_body(body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body, "application/json")
    }

    /// The requests the venue received for `route`, the clock's own excluded.
    async fn asked(server: &MockServer, route: &str) -> Vec<Request> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == route)
            .collect()
    }

    fn param(request: &Request, key: &str) -> Option<String> {
        request
            .url
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    }

    fn assert_signed(request: &Request) {
        assert!(param(request, "signature").is_some(), "{}", request.url);
        assert!(param(request, "timestamp").is_some(), "{}", request.url);
        assert_eq!(request.headers.get("X-MBX-APIKEY").unwrap(), "test-key");
    }

    fn assert_public(request: &Request) {
        assert!(param(request, "signature").is_none(), "{}", request.url);
        assert!(param(request, "timestamp").is_none(), "{}", request.url);
        assert!(request.headers.get("X-MBX-APIKEY").is_none());
    }

    // Documented: the Spot API documentation, "Query Commission Rates
    // (USER_DATA)", Response (its `//` annotations cut, nothing else edited):
    // https://github.com/binance/binance-spot-api-docs/blob/master/rest-api.md#query-commission-rates-user_data
    // Until the production tier records one, this body is `documented` in
    // `docs/responses/binance-spot.md`.
    const DOCUMENTED_COMMISSION: &str = r#"{
    "symbol": "BTCUSDT",
    "standardCommission": {
        "maker": "0.00000010",
        "taker": "0.00000020",
        "buyer": "0.00000030",
        "seller": "0.00000040"
    },
    "specialCommission": {
        "maker": "0.01000000",
        "taker": "0.02000000",
        "buyer": "0.03000000",
        "seller": "0.04000000"
    },
    "taxCommission": {
        "maker": "0.00000112",
        "taker": "0.00000114",
        "buyer": "0.00000118",
        "seller": "0.00000116"
    },
    "discount": {
        "enabledForAccount": true,
        "enabledForSymbol": true,
        "discountAsset": "BNB",
        "discount": "0.75000000"
    }
}"#;

    // Synthetic: the field names of "Get API Key Permission (USER_DATA)" in the
    // Wallet documentation (`ipRestrict`, `createTime`, `enableReading`,
    // `enableWithdrawals`, `enableInternalTransfer`, `enableMargin`,
    // `enableFutures`, `enableSpotAndMarginTrading`, and the flags this crate
    // does not read), with values chosen here. That page could not be reached
    // when this was written, so the body is not copied from it: `synthetic`
    // in `docs/responses/binance-spot.md`, until a production run records one.
    const SYNTHETIC_RESTRICTIONS: &str = r#"{
    "ipRestrict": true,
    "createTime": 1698645219000,
    "enableReading": true,
    "enableWithdrawals": false,
    "enableInternalTransfer": false,
    "enableMargin": false,
    "enableFutures": false,
    "permitsUniversalTransfer": false,
    "enableVanillaOptions": false,
    "enableFixApiTrade": false,
    "enableFixReadOnly": true,
    "enableSpotAndMarginTrading": true,
    "enablePortfolioMarginTrading": false
}"#;

    // Documented: "Symbol order book ticker", Response, a single symbol.
    const DOCUMENTED_BOOK_TICKER: &str = r#"{
    "symbol": "LTCBTC",
    "bidPrice": "4.00000000",
    "bidQty": "431.00000000",
    "askPrice": "4.00000200",
    "askQty": "9.00000000"
}"#;

    // Documented: "Recent trades list", Response.
    const DOCUMENTED_TRADES: &str = r#"[
    {
        "id": 28457,
        "price": "4.00000100",
        "qty": "12.00000000",
        "quoteQty": "48.000012",
        "time": 1499865549590,
        "isBuyerMaker": true,
        "isBestMatch": true
    }
]"#;

    // Documented: "Order book", Response (`//` annotations cut).
    const DOCUMENTED_DEPTH: &str = r#"{
    "lastUpdateId": 1027024,
    "bids": [
        [
            "4.00000000",
            "431.00000000"
        ]
    ],
    "asks": [["4.00000200", "12.00000000"]]
}"#;

    /// `exchangeInfo?symbol=` for one symbol: the documentation's symbol object
    /// ("Exchange information", Response; its `filters` are left empty there),
    /// with `filters` as the "Filters" documentation prints each one under
    /// "/exchangeInfo format". The documentation prints no whole answer, so this
    /// one is assembled from its parts: `synthetic`.
    fn exchange_info(symbol: &str, status: &str, filters: &str) -> String {
        format!(
            r#"{{
    "timezone": "UTC",
    "serverTime": 1565246363776,
    "rateLimits": [],
    "exchangeFilters": [],
    "symbols": [
        {{
            "symbol": "{symbol}",
            "status": "{status}",
            "baseAsset": "ETH",
            "baseAssetPrecision": 8,
            "quoteAsset": "BTC",
            "quotePrecision": 8,
            "quoteAssetPrecision": 8,
            "orderTypes": ["LIMIT", "MARKET"],
            "isSpotTradingAllowed": true,
            "filters": [{filters}],
            "permissions": [],
            "permissionSets": [["SPOT", "MARGIN"]]
        }}
    ]
}}"#
        )
    }

    const PRICE_FILTER: &str = r#"{"filterType": "PRICE_FILTER", "minPrice": "0.00000100", "maxPrice": "100000.00000000", "tickSize": "0.00000100"}"#;
    const LOT_SIZE: &str = r#"{"filterType": "LOT_SIZE", "minQty": "0.00100000", "maxQty": "100000.00000000", "stepSize": "0.00100000"}"#;
    const NOTIONAL: &str = r#"{"filterType": "NOTIONAL", "minNotional": "10.00000000", "applyMinToMarket": false, "maxNotional": "10000.00000000", "applyMaxToMarket": false, "avgPriceMins": 5}"#;
    const MIN_NOTIONAL: &str = r#"{"filterType": "MIN_NOTIONAL", "minNotional": "0.00100000", "applyToMarket": true, "avgPriceMins": 5}"#;

    #[tokio::test]
    async fn the_documented_commission_reads_as_exact_rates() -> Result<()> {
        let server = venue().await;
        answering(
            &server,
            ACCOUNT_COMMISSION_PATH,
            json_body(DOCUMENTED_COMMISSION),
        )
        .await;

        let commission = rest(&server).account_commission("BTCUSDT").await?;

        assert_eq!(commission.symbol, "BTCUSDT");
        assert_eq!(
            commission.standard,
            Commission {
                maker: decimal("0.00000010"),
                taker: decimal("0.00000020"),
                buyer: decimal("0.00000030"),
                seller: decimal("0.00000040"),
            }
        );
        assert_eq!(commission.special.taker, decimal("0.02"));
        assert_eq!(commission.tax.seller, decimal("0.00000116"));
        assert_eq!(
            commission.discount,
            CommissionDiscount {
                enabled_for_account: true,
                enabled_for_symbol: true,
                asset: Some("BNB".to_string()),
                rate: decimal("0.75"),
            }
        );
        // Exact: the eight decimals the venue printed, no float in between.
        assert_eq!(commission.standard.taker.to_string(), "0.00000020");
        Ok(())
    }

    /// The call is a signed GET for the symbol, with the key.
    #[tokio::test]
    async fn the_commission_read_is_a_signed_get_for_the_symbol() -> Result<()> {
        let server = venue().await;
        answering(
            &server,
            ACCOUNT_COMMISSION_PATH,
            json_body(DOCUMENTED_COMMISSION),
        )
        .await;

        rest(&server).account_commission("BTCUSDT").await?;

        let requests = asked(&server, ACCOUNT_COMMISSION_PATH).await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method.as_str(), "GET");
        assert_eq!(param(&requests[0], "symbol").as_deref(), Some("BTCUSDT"));
        assert_signed(&requests[0]);
        Ok(())
    }

    #[tokio::test]
    async fn a_commission_for_another_symbol_is_an_error() {
        let server = venue().await;
        answering(
            &server,
            ACCOUNT_COMMISSION_PATH,
            json_body(DOCUMENTED_COMMISSION),
        )
        .await;

        let err = rest(&server)
            .account_commission("ETHUSDT")
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("ETHUSDT") && err.to_string().contains("BTCUSDT"),
            "{err}"
        );
    }

    /// A rate sent as a JSON number would arrive through a float: refused. So is
    /// a field the venue leaves out, never read as zero.
    #[tokio::test]
    async fn a_commission_that_is_not_exact_strings_is_an_error_never_a_float_or_a_zero() {
        let numeric =
            DOCUMENTED_COMMISSION.replace(r#""maker": "0.00000010""#, r#""maker": 0.0000001"#);
        let no_discount = {
            let mut body: serde_json::Value = serde_json::from_str(DOCUMENTED_COMMISSION).unwrap();
            body.as_object_mut().unwrap().remove("discount");
            body.to_string()
        };
        let no_taker = DOCUMENTED_COMMISSION.replacen(r#""taker": "0.00000020","#, "", 1);
        for (what, body) in [
            ("a number", numeric),
            ("no discount", no_discount),
            ("no taker", no_taker),
        ] {
            let server = venue().await;
            answering(&server, ACCOUNT_COMMISSION_PATH, json_body(&body)).await;

            let err = rest(&server)
                .account_commission("BTCUSDT")
                .await
                .unwrap_err();

            assert!(refusal_of(&err).is_none(), "{what}: {err:#}");
            assert!(
                format!("{err:#}").contains("no readable answer"),
                "{what}: {err:#}"
            );
        }
    }

    #[tokio::test]
    async fn a_refused_commission_read_is_a_venue_refusal() {
        let server = venue().await;
        answering(
            &server,
            ACCOUNT_COMMISSION_PATH,
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -2015, "msg": "Invalid API-key, IP, or permissions for action."
            })),
        )
        .await;

        let err = rest(&server)
            .account_commission("BTCUSDT")
            .await
            .unwrap_err();

        assert_eq!(refusal_of(&err).unwrap().code, Some(-2015));
        assert!(err.to_string().contains(ACCOUNT_COMMISSION_PATH), "{err}");
    }

    #[tokio::test]
    async fn the_restrictions_a_key_has_read_as_flags() -> Result<()> {
        let server = venue().await;
        answering(
            &server,
            API_RESTRICTIONS_PATH,
            json_body(SYNTHETIC_RESTRICTIONS),
        )
        .await;

        let restrictions = rest(&server).api_restrictions().await?;

        assert_eq!(
            restrictions,
            ApiRestrictions {
                ip_restrict: true,
                enable_reading: true,
                enable_withdrawals: false,
                enable_internal_transfer: false,
                enable_spot_and_margin_trading: true,
                enable_futures: false,
                enable_margin: false,
                create_time_ms: Some(1_698_645_219_000),
            }
        );
        let requests = asked(&server, API_RESTRICTIONS_PATH).await;
        assert_eq!(requests.len(), 1);
        assert_signed(&requests[0]);
        Ok(())
    }

    /// `createTime` is the one field that may be absent; the flags are not, and
    /// a flag that is missing is an error naming it, never `false`.
    #[tokio::test]
    async fn a_missing_flag_is_an_error_and_a_missing_create_time_is_none() -> Result<()> {
        let server = venue().await;
        let without_time = SYNTHETIC_RESTRICTIONS.replace(r#""createTime": 1698645219000,"#, "");
        answering(&server, API_RESTRICTIONS_PATH, json_body(&without_time)).await;
        assert_eq!(rest(&server).api_restrictions().await?.create_time_ms, None);

        let server = venue().await;
        let without_withdrawals =
            SYNTHETIC_RESTRICTIONS.replace(r#""enableWithdrawals": false,"#, "");
        answering(
            &server,
            API_RESTRICTIONS_PATH,
            json_body(&without_withdrawals),
        )
        .await;
        let err = rest(&server).api_restrictions().await.unwrap_err();
        assert!(format!("{err:#}").contains("enableWithdrawals"), "{err:#}");
        Ok(())
    }

    /// The spot testnet has no `/sapi`: what it answers is an error carrying
    /// the refusal, never a key that reads as restricted or unrestricted.
    #[tokio::test]
    async fn a_host_with_no_sapi_is_a_refusal() {
        let server = venue().await;
        answering(
            &server,
            API_RESTRICTIONS_PATH,
            ResponseTemplate::new(404).set_body_string("Not Found"),
        )
        .await;

        let err = rest(&server).api_restrictions().await.unwrap_err();

        let refusal = refusal_of(&err).expect("a 404 is a refusal");
        assert_eq!((refusal.status, refusal.code), (404, None));
    }

    #[tokio::test]
    async fn the_documented_book_ticker_reads_as_exact_decimals() -> Result<()> {
        let server = venue().await;
        answering(&server, BOOK_TICKER_PATH, json_body(DOCUMENTED_BOOK_TICKER)).await;

        let ticker = rest(&server).book_ticker("LTCBTC").await?;

        assert_eq!(
            ticker,
            BookTicker {
                symbol: "LTCBTC".to_string(),
                bid_price: decimal("4"),
                bid_qty: decimal("431"),
                ask_price: decimal("4.000002"),
                ask_qty: decimal("9"),
            }
        );
        assert_eq!(ticker.ask_price.to_string(), "4.00000200");
        let requests = asked(&server, BOOK_TICKER_PATH).await;
        assert_eq!(param(&requests[0], "symbol").as_deref(), Some("LTCBTC"));
        assert_public(&requests[0]);
        Ok(())
    }

    /// A list where one symbol was asked for is not that symbol's ticker.
    #[tokio::test]
    async fn a_book_ticker_for_another_symbol_or_a_list_is_an_error() {
        let server = venue().await;
        answering(&server, BOOK_TICKER_PATH, json_body(DOCUMENTED_BOOK_TICKER)).await;
        let err = rest(&server).book_ticker("BTCUSDT").await.unwrap_err();
        assert!(err.to_string().contains("LTCBTC"), "{err}");

        let server = venue().await;
        answering(
            &server,
            BOOK_TICKER_PATH,
            json_body(&format!("[{DOCUMENTED_BOOK_TICKER}]")),
        )
        .await;
        assert!(rest(&server).book_ticker("LTCBTC").await.is_err());
    }

    #[tokio::test]
    async fn a_symbols_rules_read_as_step_minimums_and_status() -> Result<()> {
        let server = venue().await;
        let filters = [PRICE_FILTER, LOT_SIZE, NOTIONAL].join(",");
        answering(
            &server,
            EXCHANGE_INFO_PATH,
            json_body(&exchange_info("ETHBTC", "TRADING", &filters)),
        )
        .await;

        let rules = rest(&server).symbol_rules("ETHBTC").await?;

        assert_eq!(
            rules,
            SymbolRules {
                symbol: "ETHBTC".to_string(),
                status: "TRADING".to_string(),
                base_asset: "ETH".to_string(),
                quote_asset: "BTC".to_string(),
                lot_step: decimal("0.001"),
                min_qty: decimal("0.001"),
                min_notional: Some(decimal("10")),
                apply_min_to_market: Some(false),
            }
        );
        let requests = asked(&server, EXCHANGE_INFO_PATH).await;
        assert_eq!(param(&requests[0], "symbol").as_deref(), Some("ETHBTC"));
        assert_public(&requests[0]);
        Ok(())
    }

    /// `NOTIONAL` is the current minimum, `MIN_NOTIONAL` the one it replaced
    /// (and says `applyToMarket`); a symbol with neither has none, not zero.
    #[tokio::test]
    async fn the_minimum_notional_is_the_one_the_symbol_has_or_none() -> Result<()> {
        for (filters, minimum, apply) in [
            (
                [LOT_SIZE, MIN_NOTIONAL].join(","),
                Some(decimal("0.001")),
                Some(true),
            ),
            (
                [LOT_SIZE, MIN_NOTIONAL, NOTIONAL].join(","),
                Some(decimal("10")),
                Some(false),
            ),
            (LOT_SIZE.to_string(), None, None),
        ] {
            let server = venue().await;
            answering(
                &server,
                EXCHANGE_INFO_PATH,
                json_body(&exchange_info("ETHBTC", "TRADING", &filters)),
            )
            .await;

            let rules = rest(&server).symbol_rules("ETHBTC").await?;

            assert_eq!(
                (rules.min_notional, rules.apply_min_to_market),
                (minimum, apply),
                "{filters}"
            );
        }
        Ok(())
    }

    /// A status that does not trade is reported as the venue says it; deciding
    /// what to do with it is the caller's.
    #[tokio::test]
    async fn a_symbol_that_does_not_trade_says_its_status() -> Result<()> {
        let server = venue().await;
        answering(
            &server,
            EXCHANGE_INFO_PATH,
            json_body(&exchange_info("ETHBTC", "BREAK", LOT_SIZE)),
        )
        .await;

        assert_eq!(rest(&server).symbol_rules("ETHBTC").await?.status, "BREAK");
        Ok(())
    }

    /// Rules this crate rounds by must be there: no `LOT_SIZE`, a step of zero
    /// and a symbol the answer does not hold are each an error, never a default.
    #[tokio::test]
    async fn rules_it_cannot_round_by_are_an_error() {
        for (what, body, expected) in [
            (
                "no lot size",
                exchange_info("ETHBTC", "TRADING", PRICE_FILTER),
                "no LOT_SIZE filter",
            ),
            (
                "a zero step",
                exchange_info(
                    "ETHBTC",
                    "TRADING",
                    r#"{"filterType": "LOT_SIZE", "minQty": "0.00000000", "maxQty": "9000.00000000", "stepSize": "0.00000000"}"#,
                ),
                "stepSize of zero",
            ),
            (
                "another symbol",
                exchange_info("BTCUSDT", "TRADING", LOT_SIZE),
                "no row for ETHBTC",
            ),
        ] {
            let server = venue().await;
            answering(&server, EXCHANGE_INFO_PATH, json_body(&body)).await;

            let err = rest(&server).symbol_rules("ETHBTC").await.unwrap_err();

            assert!(format!("{err:#}").contains(expected), "{what}: {err:#}");
        }
    }

    #[tokio::test]
    async fn an_unknown_symbol_is_a_refusal_carrying_the_venues_code() {
        let server = venue().await;
        answering(
            &server,
            EXCHANGE_INFO_PATH,
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -1121, "msg": "Invalid symbol."
            })),
        )
        .await;

        let err = rest(&server).symbol_rules("NOPE").await.unwrap_err();

        assert_eq!(refusal_of(&err).unwrap().code, Some(-1121));
    }

    #[tokio::test]
    async fn the_documented_trades_read_as_exact_decimals_and_a_limit() -> Result<()> {
        let server = venue().await;
        answering(&server, TRADES_PATH, json_body(DOCUMENTED_TRADES)).await;

        let trades = rest(&server).recent_trades("BNBBTC", 1_000).await?;

        assert_eq!(
            trades,
            vec![PublicTrade {
                id: 28_457,
                price: decimal("4.000001"),
                qty: decimal("12"),
                time_ms: 1_499_865_549_590,
                is_buyer_maker: true,
            }]
        );
        assert_eq!(trades[0].price.to_string(), "4.00000100");
        let requests = asked(&server, TRADES_PATH).await;
        assert_eq!(param(&requests[0], "symbol").as_deref(), Some("BNBBTC"));
        assert_eq!(param(&requests[0], "limit").as_deref(), Some("1000"));
        assert_public(&requests[0]);
        Ok(())
    }

    #[tokio::test]
    async fn the_documented_book_reads_as_levels_best_first_with_its_update_id() -> Result<()> {
        let server = venue().await;
        answering(&server, DEPTH_PATH, json_body(DOCUMENTED_DEPTH)).await;

        let book = rest(&server).order_book("BNBBTC", 5).await?;

        assert_eq!(book.last_update_id, 1_027_024);
        assert_eq!(book.bids, vec![(decimal("4"), decimal("431"))]);
        assert_eq!(book.asks, vec![(decimal("4.000002"), decimal("12"))]);
        assert_eq!(book.asks[0].0.to_string(), "4.00000200");
        let requests = asked(&server, DEPTH_PATH).await;
        assert_eq!(param(&requests[0], "symbol").as_deref(), Some("BNBBTC"));
        assert_eq!(param(&requests[0], "limit").as_deref(), Some("5"));
        assert_public(&requests[0]);
        Ok(())
    }

    /// A side of several levels keeps the order the venue sent, which is checked
    /// to be best first; one that is not is an error, since a depth computed
    /// from it would be wrong without saying so.
    #[tokio::test]
    async fn a_book_side_out_of_price_order_is_an_error() -> Result<()> {
        let book = |bids: &str, asks: &str| {
            format!(r#"{{"lastUpdateId": 7, "bids": {bids}, "asks": {asks}}}"#)
        };
        let server = venue().await;
        answering(
            &server,
            DEPTH_PATH,
            json_body(&book(
                r#"[["10.5","1"],["10.4","2"],["10.1","3"]]"#,
                r#"[["10.6","1"],["10.7","2"]]"#,
            )),
        )
        .await;
        let ordered = rest(&server).order_book("BNBBTC", 5).await?;
        assert_eq!(ordered.bids.len() + ordered.asks.len(), 5);

        for (what, body) in [
            (
                "bids ascending",
                book(r#"[["10.1","1"],["10.4","1"]]"#, "[]"),
            ),
            (
                "asks descending",
                book("[]", r#"[["10.7","1"],["10.6","1"]]"#),
            ),
            (
                "a repeated price",
                book(r#"[["10.4","1"],["10.4","2"]]"#, "[]"),
            ),
        ] {
            let server = venue().await;
            answering(&server, DEPTH_PATH, json_body(&body)).await;
            let err = rest(&server).order_book("BNBBTC", 5).await.unwrap_err();
            assert!(
                err.to_string().contains("not in strict price order"),
                "{what}: {err}"
            );
        }
        Ok(())
    }

    /// A side with nothing on it is empty, which is what the venue said.
    #[tokio::test]
    async fn an_empty_side_is_empty() -> Result<()> {
        let server = venue().await;
        answering(
            &server,
            DEPTH_PATH,
            json_body(r#"{"lastUpdateId": 9, "bids": [], "asks": []}"#),
        )
        .await;

        let book = rest(&server).order_book("BNBBTC", 5).await?;

        assert_eq!((book.bids.len(), book.asks.len()), (0, 0));
        Ok(())
    }

    /// A request the venue would refuse for its shape is refused here, and
    /// nothing is sent.
    #[tokio::test]
    async fn a_request_with_no_symbol_or_a_limit_out_of_range_sends_nothing() {
        let server = MockServer::start().await;
        let rest = rest(&server);

        assert!(rest.account_commission("").await.is_err());
        assert!(rest.book_ticker("").await.is_err());
        assert!(rest.symbol_rules("").await.is_err());
        assert!(rest.recent_trades("", 10).await.is_err());
        assert!(rest.order_book("", 10).await.is_err());
        for limit in [0, 1_001] {
            let err = rest.recent_trades("BNBBTC", limit).await.unwrap_err();
            assert!(err.to_string().contains("nothing was sent"), "{err}");
        }
        for limit in [0, 5_001] {
            let err = rest.order_book("BNBBTC", limit).await.unwrap_err();
            assert!(err.to_string().contains("nothing was sent"), "{err}");
        }

        assert_eq!(server.received_requests().await.unwrap().len(), 0);
    }

    /// A read the venue answers with something that is not its body is an error
    /// that is not a refusal: nothing says the venue turned the request down.
    #[tokio::test]
    async fn a_read_with_no_readable_answer_is_not_a_refusal() {
        let server = venue().await;
        answering(
            &server,
            DEPTH_PATH,
            ResponseTemplate::new(503).set_body_string("Unknown error"),
        )
        .await;
        answering(&server, TRADES_PATH, json_body("{not json")).await;

        let book = rest(&server).order_book("BNBBTC", 5).await.unwrap_err();
        let trades = rest(&server).recent_trades("BNBBTC", 5).await.unwrap_err();

        assert!(refusal_of(&book).is_none(), "{book:#}");
        assert!(refusal_of(&trades).is_none(), "{trades:#}");
    }

    /// `BinanceLive` hands out the client it trades through, so a caller reads
    /// the account's commission, the book and the rules with the same keys,
    /// connections and clock.
    #[tokio::test]
    async fn binance_live_exposes_its_rest_client_for_these_reads() -> Result<()> {
        use crate::cex::BinanceLive;
        use std::collections::HashMap;

        let server = venue().await;
        answering(&server, BOOK_TICKER_PATH, json_body(DOCUMENTED_BOOK_TICKER)).await;
        let live = BinanceLive::new(rest(&server), HashMap::new());

        let ticker = live.rest().book_ticker("LTCBTC").await?;

        assert_eq!(ticker.bid_price, decimal("4"));
        Ok(())
    }
}
