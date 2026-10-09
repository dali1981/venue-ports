//! Finding out what became of an order, the same way on Binance spot and
//! USDⓈ-M futures: both answer `GET …/order?origClientOrderId=` with the
//! order's status, both list its trade lines with `commission` and
//! `commissionAsset`, and both use the same order statuses.
//!
//! Nothing here returns a plain error for an order the venue may have
//! accepted: what cannot be read becomes an `OrderStateUnknown` whose
//! context says why ([`settled`], [`unreadable_fill`]), or an explanation
//! for the caller to wrap in one ([`trade_lines`], [`single_commission`]).
//! See `SPEC.md` §6.

use crate::cex::binance::client::{ApiError, BinanceClient, NO_SUCH_ORDER};
use crate::cex::{CexTrade, OrderStateUnknown};
use crate::Provenance;
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

/// One status query: `order_path` asked about the order placed under
/// `client_order_id`. [`track`] repeats it until the order settles; a caller
/// that wants one answer reads it once.
pub(crate) async fn read_order<T: VenueOrder>(
    client: &BinanceClient,
    order_path: &str,
    symbol: &str,
    client_order_id: &str,
) -> Result<T, ApiError> {
    let params = [
        ("symbol", symbol.to_string()),
        ("origClientOrderId", client_order_id.to_string()),
    ];
    client.signed::<T>(Method::GET, order_path, &params).await
}

/// What asking the venue about an order came to.
enum Tracked<T> {
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
/// then "no such order" is conclusive only from a query *sent* after
/// `recvWindow` has passed for that request — until then the venue could
/// still accept it — and querying goes on until one such query has been
/// answered, however long `poll_timeout` is.
///
/// A query that fails is tried again until then; only after that does the
/// order become [`Tracked::Unknown`].
async fn track<T: VenueOrder>(
    client: &BinanceClient,
    order_path: &str,
    symbol: &str,
    client_order_id: &str,
    known_order_id: Option<u64>,
    lost_signed_at: Option<Instant>,
) -> Tracked<T> {
    let timings = client.timings().clone();
    let deadline = Instant::now() + timings.poll_timeout;
    let window_ends = lost_signed_at.map(|signed_at| client.recv_window_ends(signed_at));
    let mut order_ref = known_order_id;
    let mut problem = anyhow!("the venue has not answered yet");

    loop {
        // Taken before the query is signed and sent, so a query counted as
        // asked after the window was certainly sent after it.
        let asked_at = Instant::now();
        let asked_after_window = window_ends.is_none_or(|ends| asked_at > ends);
        match read_order::<T>(client, order_path, symbol, client_order_id).await {
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
                if window_ends.is_some() && asked_after_window {
                    return Tracked::NeverAccepted;
                }
                problem = anyhow::Error::from(err)
                    .context("the venue had no such order yet, and recvWindow had not passed");
            }
            Err(err) => {
                problem = anyhow::Error::from(err).context("the order-status query failed");
            }
        }
        if Instant::now() >= deadline && asked_after_window {
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
    /// The trade's id: `tradeId` on a `fills` entry, `id` on a
    /// `myTrades`/`userTrades` row.
    #[serde(rename = "tradeId", alias = "id", default)]
    pub trade_id: Option<u64>,
}

impl TradeLine {
    pub(crate) fn trade(&self) -> CexTrade {
        CexTrade {
            trade_id: self.trade_id,
            price: self.price,
            qty: self.qty,
            commission: self.commission,
            commission_asset: self.commission_asset.clone(),
        }
    }
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
            Err(err) => anyhow::Error::from(err).context(format!("reading {trades_path}")),
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

/// Follows the answer to a placing call (`placed`) to the order in a
/// terminal state, or to the error `execute` returns:
///
/// - answered with a terminal status: that order;
/// - answered with a status that is still working: tracked by client order
///   id until it settles;
/// - answer lost: tracked by client order id, with "no such order"
///   conclusive only once `recvWindow` has passed ([`track`]);
/// - refused: an error carrying the venue's code and message, and a
///   [`VenueRefusal`](crate::cex::VenueRefusal) a caller reads them from by
///   type. Nothing filled;
/// - never sent: a plain error. Nothing filled.
///
/// Anything that cannot be read once the venue accepted the order, or may
/// have, is an `OrderStateUnknown` whose context says why. `what` names the
/// order in every message.
pub(crate) async fn settled<T: VenueOrder>(
    client: &BinanceClient,
    order_path: &str,
    symbol: &str,
    client_order_id: &str,
    what: &str,
    placed: Result<T, ApiError>,
) -> anyhow::Result<T> {
    let (tracked, lost) = match placed {
        Ok(order) if is_terminal(order.status()) => return Ok(order),
        Ok(order) => {
            let tracked = track(
                client,
                order_path,
                symbol,
                client_order_id,
                Some(order.order_id()),
                None,
            )
            .await;
            (tracked, None)
        }
        Err(ApiError::Lost { signed_at, cause }) => {
            let tracked = track(
                client,
                order_path,
                symbol,
                client_order_id,
                None,
                Some(signed_at),
            )
            .await;
            (tracked, Some(cause))
        }
        Err(ApiError::Refused(refusal)) => {
            let why = format!("{what} was not placed, nothing filled: {refusal}");
            return Err(refusal.because(Provenance::Landed, why));
        }
        Err(err) => bail!("{what} was not placed, nothing filled: {err}"),
    };

    match tracked {
        Tracked::Terminal(order) => Ok(order),
        Tracked::NeverAccepted => bail!(
            "{what}: the answer to the placing call was lost, and the venue still had no such \
             order once recvWindow had passed, so it never accepted it; nothing filled"
        ),
        Tracked::Unknown { order_ref, cause } => {
            let why = match lost {
                Some(lost) => format!(
                    "{what}: the answer to the placing call was lost ({lost:#}), and what became \
                     of the order could not be read: {cause:#}"
                ),
                None => format!(
                    "{what}: the venue accepted the order, but what became of it could not be \
                     read: {cause:#}"
                ),
            };
            Err(OrderStateUnknown {
                symbol: symbol.to_string(),
                client_order_id: client_order_id.to_string(),
                order_ref,
            }
            .because(Provenance::Landed, why))
        }
    }
}

/// The error for an order that filled `executed_qty` as order `order_ref`
/// but whose fill could not be read, for the reason in `why`.
pub(crate) fn unreadable_fill(
    symbol: &str,
    client_order_id: &str,
    order_ref: u64,
    executed_qty: Decimal,
    why: anyhow::Error,
) -> anyhow::Error {
    OrderStateUnknown {
        symbol: symbol.to_string(),
        client_order_id: client_order_id.to_string(),
        order_ref: Some(order_ref),
    }
    .because(
        Provenance::Landed,
        why.context(format!(
            "order {order_ref} filled {executed_qty} but the fill could not be read"
        )),
    )
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
            trade_id: None,
        }
    }

    #[test]
    fn a_trade_id_is_read_from_a_fill_and_from_a_trade_list_row() {
        let fill: TradeLine = serde_json::from_str(
            r#"{"price":"4.0","qty":"1.0","commission":"0.004","commissionAsset":"USDT","tradeId":56}"#,
        )
        .unwrap();
        let row: TradeLine = serde_json::from_str(
            r#"{"symbol":"BNBBTC","id":28457,"orderId":100234,"orderListId":-1,"price":"4.0","qty":"12.0",
                "quoteQty":"48.0","commission":"10.1","commissionAsset":"BNB","time":1499865549590,
                "isBuyer":true,"isMaker":false,"isBestMatch":true}"#,
        )
        .unwrap();
        assert_eq!((fill.trade_id, fill.order_id), (Some(56), None));
        assert_eq!((row.trade_id, row.order_id), (Some(28457), Some(100234)));
        assert_eq!(row.trade().commission_asset, "BNB");
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
