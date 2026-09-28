//! Finding out what became of an order, the same way on Binance spot and
//! USDⓈ-M futures: both answer `GET …/order?origClientOrderId=` with the
//! order's status, both list its trade lines with `commission` and
//! `commissionAsset`, and both use the same order statuses.
//!
//! Nothing here returns a plain error for an order the venue may have
//! accepted. What cannot be read becomes [`Tracked::Unknown`] or an
//! explanation for the caller to wrap in `OrderStateUnknown`
//! (`SPEC.md` §6).

use crate::cex::binance::client::{ApiError, BinanceClient, NO_SUCH_ORDER};
use anyhow::{anyhow, bail};
use reqwest::Method;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::time::Instant;

/// Whether an order in `status` will change no further. The same set on
/// spot and futures; see
/// <https://developers.binance.com/docs/binance-spot-api-docs/enums#order-status-status>.
/// Anything else, a status this crate does not know included, is treated
/// as still working, so it is polled until it settles or the adapter gives
/// up with `OrderStateUnknown`, never read as a result.
pub(crate) fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "FILLED" | "CANCELED" | "REJECTED" | "EXPIRED" | "EXPIRED_IN_MATCH"
    )
}

/// An order as the venue's order-status endpoint reports it.
pub(crate) trait VenueOrder: DeserializeOwned {
    fn order_id(&self) -> u64;
    fn status(&self) -> &str;
}

/// What asking the venue about an order came to.
pub(crate) enum Tracked<T> {
    /// The venue reports the order in a terminal state.
    Terminal(T),
    /// The venue still had no such order once `recvWindow` had passed for
    /// the lost request: it never accepted it, and nothing filled.
    NeverAccepted,
    /// The order's outcome could not be read in time.
    Unknown {
        order_ref: Option<u64>,
        cause: anyhow::Error,
    },
}

/// Queries `order_path` for `client_order_id` until the venue reports a
/// terminal state, for up to `poll_timeout`.
///
/// `known_order_id` is set when the venue acknowledged the order (its
/// placing call answered, with a status that is not terminal yet).
/// `lost_signed_at` is set instead when the placing call's answer was lost:
/// then "no such order" is only conclusive once `recvWindow` has passed for
/// that request, since until then the venue could still accept it, and the
/// query keeps going at least that long.
///
/// A query that fails is tried again until the deadline; only then does
/// the order become [`Tracked::Unknown`].
pub(crate) async fn track<T: VenueOrder>(
    client: &BinanceClient,
    order_path: &str,
    symbol: &str,
    client_order_id: &str,
    known_order_id: Option<u64>,
    lost_signed_at: Option<Instant>,
) -> Tracked<T> {
    let timings = client.timings().clone();
    let mut deadline = Instant::now() + timings.poll_timeout;
    if let Some(signed_at) = lost_signed_at {
        // Leave room for one last query after the window has passed.
        let window_passes = client.recv_window_ends(signed_at) + timings.poll_interval * 2;
        deadline = deadline.max(window_passes);
    }
    let params = [
        ("symbol", symbol.to_string()),
        ("origClientOrderId", client_order_id.to_string()),
    ];
    let mut order_ref = known_order_id;
    let mut problem = anyhow!("the venue has not answered yet");

    loop {
        match client.signed::<T>(Method::GET, order_path, &params).await {
            Ok(order) => {
                order_ref = Some(order.order_id());
                if is_terminal(order.status()) {
                    return Tracked::Terminal(order);
                }
                problem = anyhow!(
                    "order {} was still {} when this adapter stopped waiting",
                    order.order_id(),
                    order.status()
                );
            }
            Err(err) if err.code() == Some(NO_SUCH_ORDER) && order_ref.is_none() => {
                if let Some(signed_at) = lost_signed_at {
                    if client.recv_window_passed(signed_at) {
                        return Tracked::NeverAccepted;
                    }
                }
                problem = anyhow::Error::new(err)
                    .context("the venue had no such order yet, and recvWindow had not passed");
            }
            Err(err) => {
                problem = anyhow::Error::new(err).context("the order-status query failed");
            }
        }
        if Instant::now() >= deadline {
            return Tracked::Unknown {
                order_ref,
                cause: problem,
            };
        }
        tokio::time::sleep(timings.poll_interval).await;
    }
}

/// One trade line of an order: a `fills` entry of a spot `FULL` response,
/// or a row of spot `myTrades` or futures `userTrades`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TradeLine {
    pub price: Decimal,
    pub qty: Decimal,
    pub commission: Decimal,
    #[serde(rename = "commissionAsset")]
    pub commission_asset: String,
    /// Present on `myTrades`/`userTrades` rows, absent on `fills`.
    #[serde(rename = "orderId", default)]
    pub order_id: Option<u64>,
}

/// Reads `trades_path` for `order_id` until its lines add up to
/// `executed_qty`, for up to `trades_timeout`. The lines can lag the fill
/// by a moment, and a fill read from only some of its lines would carry a
/// commission that is too small, so an incomplete set is never returned.
/// The error explains what was missing; the caller wraps it in
/// `OrderStateUnknown`.
pub(crate) async fn trade_lines(
    client: &BinanceClient,
    trades_path: &str,
    symbol: &str,
    order_id: u64,
    executed_qty: Decimal,
) -> anyhow::Result<Vec<TradeLine>> {
    let timings = client.timings().clone();
    let deadline = Instant::now() + timings.trades_timeout;
    let params = [
        ("symbol", symbol.to_string()),
        ("orderId", order_id.to_string()),
        ("limit", "1000".to_string()),
    ];
    loop {
        let problem = match client
            .signed::<Vec<TradeLine>>(Method::GET, trades_path, &params)
            .await
        {
            Ok(lines) => {
                let lines: Vec<TradeLine> = lines
                    .into_iter()
                    .filter(|line| line.order_id.is_none_or(|id| id == order_id))
                    .collect();
                let listed: Decimal = lines.iter().map(|line| line.qty).sum();
                if listed == executed_qty {
                    return Ok(lines);
                }
                anyhow!(
                    "{trades_path} listed {listed} of the {executed_qty} order {order_id} filled"
                )
            }
            Err(err) => anyhow::Error::new(err).context(format!("reading {trades_path}")),
        };
        if Instant::now() >= deadline {
            return Err(problem.context(format!(
                "the trade lines of order {order_id} could not be read within {} ms, \
                 so its commission is unknown",
                timings.trades_timeout.as_millis()
            )));
        }
        tokio::time::sleep(timings.poll_interval).await;
    }
}

/// The total commission of `lines` and the one asset it was charged in.
///
/// `CexFill` holds one commission asset, so commission charged in more
/// than one (BNB and the quote asset in one order, for example) cannot be
/// reported: it is an error for the caller to wrap in `OrderStateUnknown`,
/// never a fill that silently drops the second asset (README defect 2).
/// Lines charged nothing do not count as an asset.
pub(crate) fn single_commission(lines: &[TradeLine]) -> anyhow::Result<(Decimal, String)> {
    let charged: BTreeSet<&str> = lines
        .iter()
        .filter(|line| !line.commission.is_zero())
        .map(|line| line.commission_asset.as_str())
        .collect();
    let asset = match charged.len() {
        0 => lines
            .first()
            .map(|line| line.commission_asset.clone())
            .ok_or_else(|| anyhow!("the order has no trade lines to read a commission from"))?,
        1 => charged.into_iter().next().unwrap().to_string(),
        _ => bail!(
            "commission was charged in more than one asset ({}), which CexFill cannot hold",
            charged.into_iter().collect::<Vec<_>>().join(", ")
        ),
    };
    let commission = lines.iter().map(|line| line.commission).sum();
    Ok((commission, asset))
}

/// Converts an [`ApiError`] from a placing call that the venue refused, or
/// that never left this process, into the plain error `execute` returns:
/// nothing filled.
pub(crate) fn refused(err: ApiError, what: String) -> anyhow::Error {
    anyhow!("{what} was not placed, nothing filled: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn line(qty: &str, commission: &str, asset: &str) -> TradeLine {
        TradeLine {
            price: Decimal::from(100),
            qty: Decimal::from_str(qty).unwrap(),
            commission: Decimal::from_str(commission).unwrap(),
            commission_asset: asset.to_string(),
            order_id: None,
        }
    }

    #[test]
    fn commission_in_one_asset_is_summed() {
        let (commission, asset) =
            single_commission(&[line("1", "0.1", "USDT"), line("2", "0.2", "USDT")]).unwrap();
        assert_eq!(commission, Decimal::from_str("0.3").unwrap());
        assert_eq!(asset, "USDT");
    }

    #[test]
    fn commission_in_two_assets_is_refused() {
        let err =
            single_commission(&[line("1", "0.1", "USDT"), line("1", "0.001", "BNB")]).unwrap_err();
        assert!(err.to_string().contains("BNB, USDT"), "{err}");
    }

    #[test]
    fn a_line_charged_nothing_does_not_count_as_a_second_asset() {
        let (commission, asset) =
            single_commission(&[line("1", "0", "BNB"), line("1", "0.1", "USDT")]).unwrap();
        assert_eq!(commission, Decimal::from_str("0.1").unwrap());
        assert_eq!(asset, "USDT");
    }

    #[test]
    fn a_fee_free_order_reports_zero_in_its_lines_asset() {
        let (commission, asset) = single_commission(&[line("1", "0", "FDUSD")]).unwrap();
        assert!(commission.is_zero());
        assert_eq!(asset, "FDUSD");
    }

    #[test]
    fn terminal_statuses_are_the_documented_five() {
        for status in [
            "FILLED",
            "CANCELED",
            "REJECTED",
            "EXPIRED",
            "EXPIRED_IN_MATCH",
        ] {
            assert!(is_terminal(status));
        }
        for status in ["NEW", "PARTIALLY_FILLED", "PENDING_NEW", "SOMETHING_NEW"] {
            assert!(!is_terminal(status));
        }
    }
}
