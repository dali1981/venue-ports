//! `GET /fapi/v1/exchangeInfo` → per-symbol `MARKET_LOT_SIZE` and
//! `MIN_NOTIONAL`, read once at `connect`, and the check every order passes
//! before it is sent.
//!
//! A market order is bound by `MARKET_LOT_SIZE`, not `LOT_SIZE`. Futures'
//! `MIN_NOTIONAL` carries its minimum in a field named `notional` (spot's
//! is `minNotional`).

use anyhow::{anyhow, bail, Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;

#[derive(Debug, Deserialize)]
pub(crate) struct ExchangeInfo {
    symbols: Vec<SymbolInfo>,
}

#[derive(Debug, Deserialize)]
struct SymbolInfo {
    symbol: String,
    status: String,
    #[serde(rename = "contractType")]
    contract_type: String,
    /// Each filter is an object tagged by `filterType`, with fields of its
    /// own; only two are read.
    filters: Vec<serde_json::Value>,
}

/// What a market order for one symbol must satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SymbolFilters {
    pub(crate) step_size: Decimal,
    pub(crate) min_qty: Decimal,
    pub(crate) max_qty: Decimal,
    pub(crate) min_notional: Decimal,
}

/// The filters of each of `symbols`, refusing any that is missing, not
/// `TRADING`, not `PERPETUAL`, or without a usable `MARKET_LOT_SIZE` or
/// `MIN_NOTIONAL` — at the start, not on the first order.
pub(crate) fn symbol_filters(
    info: &ExchangeInfo,
    symbols: &[&str],
) -> Result<HashMap<String, SymbolFilters>> {
    let mut filters = HashMap::new();
    for &symbol in symbols {
        let listed = info
            .symbols
            .iter()
            .find(|listed| listed.symbol == symbol)
            .ok_or_else(|| anyhow!("{symbol} is not listed on this venue"))?;
        if listed.status != "TRADING" {
            bail!("{symbol} is not trading (status {})", listed.status);
        }
        if listed.contract_type != "PERPETUAL" {
            bail!(
                "{symbol} is not a perpetual (contractType {})",
                listed.contract_type
            );
        }
        let read = || -> Result<SymbolFilters> {
            let lot = filter(&listed.filters, "MARKET_LOT_SIZE")?;
            let notional = filter(&listed.filters, "MIN_NOTIONAL")?;
            let parsed = SymbolFilters {
                step_size: decimal(lot, "stepSize")?,
                min_qty: decimal(lot, "minQty")?,
                max_qty: decimal(lot, "maxQty")?,
                min_notional: decimal(notional, "notional")?,
            };
            if parsed.step_size <= Decimal::ZERO {
                bail!("MARKET_LOT_SIZE.stepSize is {}", parsed.step_size);
            }
            Ok(parsed)
        };
        let parsed = read().with_context(|| format!("reading {symbol}'s filters"))?;
        filters.insert(symbol.to_string(), parsed);
    }
    Ok(filters)
}

fn filter<'a>(
    filters: &'a [serde_json::Value],
    filter_type: &str,
) -> Result<&'a serde_json::Value> {
    filters
        .iter()
        .find(|filter| filter["filterType"] == filter_type)
        .ok_or_else(|| anyhow!("no {filter_type} filter"))
}

/// A decimal field, sent as a string (as Binance does) or a number.
fn decimal(filter: &serde_json::Value, field: &str) -> Result<Decimal> {
    let value = &filter[field];
    let text = match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Number(number) => number.to_string(),
        _ => bail!("{} has no {field}", filter["filterType"]),
    };
    Decimal::from_str(&text).with_context(|| format!("{field} {text:?} is not a decimal"))
}

impl SymbolFilters {
    /// The venue-ready quantity for an order of `quantity`, or why it
    /// cannot be sent: rounded **down** to the step, then refused below
    /// `minQty` or above `maxQty`, then refused when `quantity ×
    /// quoted_price` is below `MIN_NOTIONAL`, except for reduce-only
    /// orders, since a small remaining position must always be closable.
    /// `quoted_price` is only the estimate for that check, never part of
    /// the order.
    pub(crate) fn validate(
        &self,
        symbol: &str,
        quantity: Decimal,
        quoted_price: Decimal,
        reduce_only: bool,
    ) -> Result<Decimal> {
        let rounded = ((quantity / self.step_size).floor() * self.step_size).normalize();
        if rounded < self.min_qty {
            bail!(
                "quantity {quantity} of {symbol} rounds down to {rounded} at step {}, below the \
                 minimum {}",
                self.step_size,
                self.min_qty
            );
        }
        if rounded > self.max_qty {
            bail!(
                "quantity {rounded} of {symbol} is above the maximum {} for a market order",
                self.max_qty
            );
        }
        if !reduce_only && rounded * quoted_price < self.min_notional {
            bail!(
                "{rounded} {symbol} at about {quoted_price} is a notional of {}, below the \
                 minimum {} (only a reduce-only order may be smaller)",
                rounded * quoted_price,
                self.min_notional
            );
        }
        Ok(rounded)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    /// An `exchangeInfo` answer shaped like Binance's documented one, with
    /// `LOT_SIZE` deliberately different from `MARKET_LOT_SIZE`.
    pub(crate) fn exchange_info_json() -> serde_json::Value {
        let perpetual = |symbol: &str, status: &str, contract: &str| {
            serde_json::json!({
                "symbol": symbol,
                "pair": symbol,
                "contractType": contract,
                "status": status,
                "baseAsset": "BTC",
                "quoteAsset": "USDT",
                "marginAsset": "USDT",
                "quantityPrecision": 3,
                "filters": [
                    {"filterType": "PRICE_FILTER", "minPrice": "556.80", "maxPrice": "4529764", "tickSize": "0.10"},
                    {"filterType": "LOT_SIZE", "minQty": "0.001", "maxQty": "1000", "stepSize": "0.001"},
                    {"filterType": "MARKET_LOT_SIZE", "minQty": "0.002", "maxQty": "120", "stepSize": "0.002"},
                    {"filterType": "MAX_NUM_ORDERS", "limit": 200},
                    {"filterType": "MIN_NOTIONAL", "notional": "100"},
                    {"filterType": "PERCENT_PRICE", "multiplierUp": "1.0500", "multiplierDown": "0.9500", "multiplierDecimal": "4"}
                ]
            })
        };
        serde_json::json!({
            "timezone": "UTC",
            "serverTime": 1_700_000_000_000i64,
            "rateLimits": [],
            "exchangeFilters": [],
            "assets": [],
            "symbols": [
                perpetual("BTCUSDT", "TRADING", "PERPETUAL"),
                perpetual("ETHUSDT_260327", "TRADING", "CURRENT_QUARTER"),
                perpetual("OLDUSDT", "SETTLING", "PERPETUAL"),
            ]
        })
    }

    fn info() -> ExchangeInfo {
        serde_json::from_value(exchange_info_json()).unwrap()
    }

    fn btc() -> SymbolFilters {
        symbol_filters(&info(), &["BTCUSDT"]).unwrap()["BTCUSDT"].clone()
    }

    #[test]
    fn reads_market_lot_size_and_min_notional() {
        assert_eq!(
            btc(),
            SymbolFilters {
                step_size: d("0.002"),
                min_qty: d("0.002"),
                max_qty: d("120"),
                min_notional: d("100"),
            }
        );
    }

    #[test]
    fn refuses_a_symbol_that_is_missing_not_trading_or_not_perpetual() {
        for (symbol, why) in [
            ("DOGEUSDT", "not listed"),
            ("OLDUSDT", "not trading"),
            ("ETHUSDT_260327", "not a perpetual"),
        ] {
            let err = symbol_filters(&info(), &["BTCUSDT", symbol]).unwrap_err();
            assert!(err.to_string().contains(why), "{symbol}: {err}");
        }
    }

    #[test]
    fn refuses_a_symbol_without_a_market_lot_size() {
        let mut json = exchange_info_json();
        json["symbols"][0]["filters"]
            .as_array_mut()
            .unwrap()
            .retain(|filter| filter["filterType"] != "MARKET_LOT_SIZE");
        let info: ExchangeInfo = serde_json::from_value(json).unwrap();
        let err = symbol_filters(&info, &["BTCUSDT"]).unwrap_err();
        assert!(format!("{err:#}").contains("no MARKET_LOT_SIZE"), "{err:#}");
    }

    #[test]
    fn rounds_down_to_the_market_step() {
        assert_eq!(
            btc()
                .validate("BTCUSDT", d("0.0079"), d("60000"), false)
                .unwrap(),
            d("0.006")
        );
        // Exactly on a step stays put, with no trailing zeros on the wire.
        assert_eq!(
            btc()
                .validate("BTCUSDT", d("0.0100"), d("60000"), false)
                .unwrap()
                .to_string(),
            "0.01"
        );
    }

    #[test]
    fn refuses_below_the_minimum_and_above_the_maximum_quantity() {
        // 0.0039 rounds down to 0.002, the minimum; 0.0019 rounds to 0.
        assert_eq!(
            btc()
                .validate("BTCUSDT", d("0.0039"), d("60000"), false)
                .unwrap(),
            d("0.002")
        );
        // Reduce-only is exempt from the notional, never from the minimum.
        let err = btc()
            .validate("BTCUSDT", d("0.0019"), d("60000"), true)
            .unwrap_err();
        assert!(err.to_string().contains("below the minimum 0.002"), "{err}");
        let err = btc()
            .validate("BTCUSDT", d("121"), d("60000"), false)
            .unwrap_err();
        assert!(err.to_string().contains("above the maximum"), "{err}");
    }

    #[test]
    fn refuses_below_the_notional_minimum_unless_reduce_only() {
        // 0.002 × 30 000 = 60, below 100.
        let err = btc()
            .validate("BTCUSDT", d("0.002"), d("30000"), false)
            .unwrap_err();
        assert!(err.to_string().contains("below the minimum 100"), "{err}");
        assert_eq!(
            btc()
                .validate("BTCUSDT", d("0.002"), d("30000"), true)
                .unwrap(),
            d("0.002")
        );
    }
}
