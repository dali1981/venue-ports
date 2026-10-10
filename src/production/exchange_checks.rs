//! The exact checks of LV2a, and the references of an order
//! (`specs/V7-production-validation.md`, "LV2a"), as functions of what the venue
//! answered.
//!
//! Each check takes the answers it compares and nothing else, so it is tested on
//! its own: once on answers that agree, and once for each field changed. The
//! case (`lv2a_exchange`) makes the calls; this module judges them.
//!
//! | # | Check |
//! |---|---|
//! | E1 | the order's state, asked of the venue, equals the fill `execute` returned |
//! | E2 | the order's `myTrades` lines equal the fill's trades |
//! | E3 | every trade is in the venue's public trades, with the same price and quantity |
//! | E4 | the commission is the amount received × the account's rate, to the asset's scale |
//! | E5 | the account's holdings changed by exactly the signed fill |
//! | E6 | the filled quantity is a whole number of lot steps; the fill's time is the venue's `transactTime` |
//!
//! Every figure is a [`Decimal`] parsed from what the venue sent; a figure that
//! cannot be computed is an error naming it, never a zero.

use crate::balance::SpotAccountBalances;
use crate::cex::binance::order::TradeLine;
use crate::cex::{
    CexFill, CexTrade, OrderBookSnapshot, OrderSide, OrderState, PublicTrade, SymbolCommission,
    SymbolRules,
};
use crate::production::gate::Exchange;
use crate::production::sizing::is_on_step;
use crate::production::Grade;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// One unit of the last place of a commission: Binance states commissions to
/// eight decimals, and does not document how it rounds the last one.
pub(crate) fn commission_scale() -> Decimal {
    Decimal::new(1, 8)
}

fn word(side: OrderSide) -> &'static str {
    match side {
        OrderSide::Buy => "buy",
        OrderSide::Sell => "sell",
    }
}

fn mul(a: Decimal, b: Decimal, what: &str) -> Result<Decimal, String> {
    a.checked_mul(b)
        .ok_or_else(|| format!("{what}: {a} x {b} cannot be computed"))
}

fn add(a: Decimal, b: Decimal, what: &str) -> Result<Decimal, String> {
    a.checked_add(b)
        .ok_or_else(|| format!("{what}: {a} + {b} cannot be computed"))
}

/// `trades` in a fixed order, so two listings of one order's trades compare
/// whatever order the venue gave them in.
fn sorted(trades: &[CexTrade]) -> Vec<CexTrade> {
    let mut trades = trades.to_vec();
    trades.sort_by(|a, b| {
        (a.trade_id, a.price, a.qty, a.commission).cmp(&(b.trade_id, b.price, b.qty, b.commission))
    });
    trades
}

/// How two trade listings differ, or `None`.
fn trades_differ(left: &[CexTrade], right: &[CexTrade]) -> Option<String> {
    let (left, right) = (sorted(left), sorted(right));
    if left.len() != right.len() {
        return Some(format!(
            "{} trades against {} ({left:?} against {right:?})",
            left.len(),
            right.len()
        ));
    }
    left.iter()
        .zip(&right)
        .find(|(a, b)| a != b)
        .map(|(a, b)| format!("trade {:?}: {a:?} against {b:?}", a.trade_id))
}

// --- E1 ---------------------------------------------------------------------

/// E1: `state`, what the venue says became of the order, equals `fill`, what
/// `execute` returned for it: the status (a fill of everything that was asked
/// for, or part of it), the executed quantity, the price, the commission, the
/// ids and the trades. The fill's `venue_time_ms` is not compared: `execute`
/// reads the placing call's `transactTime` and a status query carries its own
/// `updateTime`.
///
/// `requested` is the quantity the venue was asked for, rounded to the lot step.
pub(crate) fn e1_order_state(
    fill: &CexFill,
    requested: Decimal,
    state: &OrderState,
) -> Result<(), String> {
    let (status, seen) = match state {
        OrderState::Filled(seen) => ("Filled", seen),
        OrderState::PartlyFilled(seen) => ("PartlyFilled", seen),
        other => return Err(format!("the venue's order status is {other:?}, not a fill")),
    };
    let mut differences = Vec::new();
    let expected = if fill.filled_qty == requested {
        "Filled"
    } else {
        "PartlyFilled"
    };
    if status != expected {
        differences.push(format!(
            "status {status}, where {} of {requested} asked for is {expected}",
            fill.filled_qty
        ));
    }
    if seen.filled_qty != fill.filled_qty {
        differences.push(format!(
            "executed quantity {} against {}",
            seen.filled_qty, fill.filled_qty
        ));
    }
    if seen.filled_price != fill.filled_price {
        differences.push(format!(
            "price {} against {}",
            seen.filled_price, fill.filled_price
        ));
    }
    if seen.commission != fill.commission || seen.commission_asset != fill.commission_asset {
        differences.push(format!(
            "commission {} {} against {} {}",
            seen.commission, seen.commission_asset, fill.commission, fill.commission_asset
        ));
    }
    if seen.order_ref != fill.order_ref {
        differences.push(format!(
            "order id {:?} against {:?}",
            seen.order_ref, fill.order_ref
        ));
    }
    if seen.client_order_id != fill.client_order_id {
        differences.push(format!(
            "client order id {:?} against {:?}",
            seen.client_order_id, fill.client_order_id
        ));
    }
    if seen.provenance != fill.provenance {
        differences.push(format!(
            "provenance {:?} against {:?}",
            seen.provenance, fill.provenance
        ));
    }
    if let Some(why) = trades_differ(&seen.trades, &fill.trades) {
        differences.push(format!("trades: {why}"));
    }
    if differences.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the order's state is not the fill execute returned: {}",
            differences.join("; ")
        ))
    }
}

// --- E2 ---------------------------------------------------------------------

/// E2: the order's `myTrades` lines equal `fill.trades`: ids, prices,
/// quantities, commissions and commission asset.
pub(crate) fn e2_my_trades(fill: &CexFill, lines: &[TradeLine]) -> Result<(), String> {
    let listed: Vec<CexTrade> = lines.iter().map(TradeLine::trade).collect();
    match trades_differ(&listed, &fill.trades) {
        None => Ok(()),
        Some(why) => Err(format!(
            "the order's myTrades lines are not the fill's trades: {why}"
        )),
    }
}

// --- E3 ---------------------------------------------------------------------

/// E3: every trade of `fill` is in `public`, the venue's list of recent public
/// trades, under the same id with the same price and quantity.
///
/// A trade the list no longer holds cannot be checked: the grade is a skip
/// (`skipped_window`), never a pass. A trade it holds with another price or
/// quantity is a failure, whatever else is missing.
pub(crate) fn e3_public_trades(fill: &CexFill, public: &[PublicTrade]) -> Grade {
    if fill.trades.is_empty() {
        return Grade::Fail("the fill lists no trades to find in the public ones".to_string());
    }
    let (mut wrong, mut missing) = (Vec::new(), Vec::new());
    for trade in &fill.trades {
        let Some(id) = trade.trade_id else {
            return Grade::Fail("a trade of the fill has no id to find it by".to_string());
        };
        match public.iter().find(|candidate| candidate.id == id) {
            None => missing.push(id),
            Some(found) if found.price == trade.price && found.qty == trade.qty => {}
            Some(found) => wrong.push(format!(
                "trade {id}: {} x {} in the fill, {} x {} in the public list",
                trade.qty, trade.price, found.qty, found.price
            )),
        }
    }
    if !wrong.is_empty() {
        return Grade::Fail(wrong.join("; "));
    }
    if !missing.is_empty() {
        return Grade::Skip(format!(
            "skipped_window: trades {missing:?} are not among the last {} public trades, so \
             they cannot be checked ({} of {} were found)",
            public.len(),
            fill.trades.len() - missing.len(),
            fill.trades.len()
        ));
    }
    Grade::Pass
}

// --- E4 ---------------------------------------------------------------------

/// What E4 made of a fill's commission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum E4 {
    /// The commission is what the rate says. The text names a difference of
    /// the last place, when there is one.
    Pass(String),
    Fail(String),
    /// The commission was paid in an asset other than the one received, so the
    /// rate cannot be applied. The rate and the asset are said.
    SkippedAsset(String),
}

/// The rate a market order of `side` pays, from the account's commission on
/// the symbol: the taker rate and the side's own (`buyer` or `seller`) of the
/// standard, special and tax blocks, added. The venue's documentation does not
/// say how the blocks combine; `specs/V7-questions.md` asks.
pub(crate) fn effective_rate(side: OrderSide, rates: &SymbolCommission) -> Result<Decimal, String> {
    let mut total = Decimal::ZERO;
    for block in [&rates.standard, &rates.special, &rates.tax] {
        let side_rate = match side {
            OrderSide::Buy => block.buyer,
            OrderSide::Sell => block.seller,
        };
        total = add(total, block.taker, "the taker rate")?;
        total = add(total, side_rate, "the side's rate")?;
    }
    Ok(total)
}

/// E4: when the commission is paid in the asset received, each trade's
/// commission is the amount it received (its quantity for a buy, its quantity
/// times its price for a sell) times the rate, to the scale of the asset, eight
/// decimals. The venue does not document its rounding of the last decimal, so a
/// difference of one unit of it is a pass that says so; a larger one is a
/// failure.
pub(crate) fn e4_commission(
    side: OrderSide,
    fill: &CexFill,
    rules: &SymbolRules,
    rates: &SymbolCommission,
) -> E4 {
    let received = match side {
        OrderSide::Buy => &rules.base_asset,
        OrderSide::Sell => &rules.quote_asset,
    };
    let rate = match effective_rate(side, rates) {
        Ok(rate) => rate,
        Err(why) => return E4::Fail(why),
    };
    if &fill.commission_asset != received {
        return E4::SkippedAsset(format!(
            "skipped_asset: the commission of {} was paid in {}, not in {received}, the asset \
             received; the {} rate is {rate}",
            fill.commission,
            fill.commission_asset,
            word(side)
        ));
    }
    let mut rounding = Vec::new();
    for trade in &fill.trades {
        let amount = match side {
            OrderSide::Buy => Ok(trade.qty),
            OrderSide::Sell => mul(trade.qty, trade.price, "the quote amount received"),
        };
        let expected = match amount.and_then(|amount| mul(amount, rate, "the commission")) {
            Ok(expected) => expected.round_dp(8),
            Err(why) => return E4::Fail(why),
        };
        let difference = (trade.commission - expected).abs();
        if difference > commission_scale() {
            return E4::Fail(format!(
                "trade {:?} paid {} {received}; {} {} at the {} rate of {rate} is {expected}",
                trade.trade_id,
                trade.commission,
                trade.qty,
                trade.price,
                word(side)
            ));
        }
        if !difference.is_zero() {
            rounding.push(format!(
                "trade {:?}: {} against {expected}",
                trade.trade_id, trade.commission
            ));
        }
    }
    E4::Pass(if rounding.is_empty() {
        String::new()
    } else {
        format!(
            "within one unit of the last decimal, which the venue does not document: {}",
            rounding.join("; ")
        )
    })
}

// --- E5 ---------------------------------------------------------------------

/// What an account holds of `asset`: free and locked.
fn held(account: &SpotAccountBalances, asset: &str) -> Decimal {
    account
        .balance(asset)
        .map_or(Decimal::ZERO, |balance| balance.free + balance.locked)
}

/// E5: between `before` and `after`, each asset's holding (free plus locked)
/// changed by exactly the signed fill: the base asset by the quantity, the quote
/// asset by the sum of price times quantity the other way, and the commission
/// taken from the asset it was paid in. An asset the fill does not touch must
/// not have changed.
pub(crate) fn e5_balances(
    side: OrderSide,
    rules: &SymbolRules,
    fill: &CexFill,
    before: &SpotAccountBalances,
    after: &SpotAccountBalances,
) -> Result<(), String> {
    let mut quote = Decimal::ZERO;
    for trade in &fill.trades {
        quote = add(
            quote,
            mul(trade.price, trade.qty, "a trade's value")?,
            "the fill's value",
        )?;
    }
    let sign = match side {
        OrderSide::Buy => Decimal::ONE,
        OrderSide::Sell => -Decimal::ONE,
    };
    let mut expected: BTreeMap<String, Decimal> = BTreeMap::new();
    *expected.entry(rules.base_asset.clone()).or_default() += sign * fill.filled_qty;
    *expected.entry(rules.quote_asset.clone()).or_default() -= sign * quote;
    *expected.entry(fill.commission_asset.clone()).or_default() -= fill.commission;

    let mut assets: Vec<&str> = before
        .balances
        .iter()
        .chain(&after.balances)
        .map(|balance| balance.asset.as_str())
        .chain(expected.keys().map(String::as_str))
        .collect();
    assets.sort_unstable();
    assets.dedup();
    let wrong: Vec<String> = assets
        .into_iter()
        .filter_map(|asset| {
            let change = held(after, asset) - held(before, asset);
            let wanted = expected.get(asset).copied().unwrap_or(Decimal::ZERO);
            (change != wanted)
                .then(|| format!("{asset} changed by {change}, the fill says {wanted}"))
        })
        .collect();
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the account did not change by the fill: {}",
            wrong.join("; ")
        ))
    }
}

// --- E6 ---------------------------------------------------------------------

/// E6: the filled quantity is a whole number of lot steps, and the fill's
/// `venue_time_ms` is the `transactTime` of the venue's reply to the placing
/// call, read from the reply as the venue sent it (`raw_reply`).
pub(crate) fn e6_step_and_time(
    fill: &CexFill,
    step: Decimal,
    raw_reply: Option<&str>,
) -> Result<(), String> {
    let mut wrong = Vec::new();
    if !is_on_step(fill.filled_qty, step) {
        wrong.push(format!(
            "the filled quantity {} is not a whole number of steps of {step}",
            fill.filled_qty
        ));
    }
    match raw_reply {
        None => wrong.push(
            "the venue's reply to the placing call was not seen, so its transactTime cannot be \
             read"
                .to_string(),
        ),
        Some(body) => {
            let transact_time = serde_json::from_str::<Value>(body)
                .ok()
                .and_then(|reply| reply["transactTime"].as_u64());
            match (transact_time, fill.venue_time_ms) {
                (Some(venue), Some(read)) if venue == read => {}
                (Some(venue), read) => wrong.push(format!(
                    "the venue's transactTime is {venue}, the fill's venue_time_ms is {read:?}"
                )),
                (None, _) => {
                    wrong.push("the venue's reply to the placing call has no transactTime".into())
                }
            }
        }
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    }
}

// --- the references of an order -----------------------------------------------

/// The book read just before an order is sent, and when.
#[derive(Debug, Clone)]
pub(crate) struct Touch {
    pub(crate) snapshot: OrderBookSnapshot,
    /// The local clock (ns since the epoch) when the read left, and when its
    /// answer had been read.
    pub(crate) read_sent_ns: u128,
    pub(crate) read_returned_ns: Option<u128>,
}

fn ns(value: u128) -> Value {
    u64::try_from(value).map_or(Value::Null, |value| json!(value))
}

fn level(level: Option<&(Decimal, Decimal)>) -> Value {
    level.map_or(
        Value::Null,
        |(price, quantity)| json!({"price": price.to_string(), "qty": quantity.to_string()}),
    )
}

/// The levels of `snapshot` that `quantity` would have used on the side an
/// order of `side` takes (asks for a buy, bids for a sell), best first, and what
/// the snapshot's levels were too few to hold.
fn depth_walked(side: OrderSide, snapshot: &OrderBookSnapshot, quantity: Decimal) -> Value {
    let (taken, levels) = match side {
        OrderSide::Buy => ("asks", &snapshot.asks),
        OrderSide::Sell => ("bids", &snapshot.bids),
    };
    let mut remaining = quantity;
    let mut walked = Vec::new();
    for (price, available) in levels {
        if remaining.is_zero() {
            break;
        }
        let used = remaining.min(*available);
        walked.push(json!({
            "price": price.to_string(),
            "level_qty": available.to_string(),
            "used_qty": used.to_string(),
        }));
        remaining -= used;
    }
    json!({
        "side": taken,
        "levels": walked,
        "beyond_snapshot_qty": remaining.normalize().to_string(),
    })
}

/// The `references` of an order that was sent (`specs/V7-production-validation.md`,
/// "LV2a"): every reference that exists at the venue when the order is placed,
/// and the answer. A reference that does not exist is `null`, never zero: the
/// fields that come from the fill are `null` for an order that did not fill,
/// and `touch_at_send` is `null` when the book could not be read.
///
/// These are the references of the consumer's terms 5 (the touch at send to the
/// average fill) and 6 (commission). Term 4 needs a decision's booked touch,
/// and this tier has no decision: `booked_touch` is `null`.
pub(crate) fn order_references(
    side: OrderSide,
    touch: Option<&Touch>,
    sent: Option<&Exchange>,
    fill: Option<&CexFill>,
    rates: Option<&SymbolCommission>,
) -> Value {
    let touch_at_send = touch.map(|touch| {
        json!({
            "best_bid": level(touch.snapshot.bids.first()),
            "best_ask": level(touch.snapshot.asks.first()),
            "last_update_id": touch.snapshot.last_update_id,
            "read_sent_ns": ns(touch.read_sent_ns),
            "read_returned_ns": touch.read_returned_ns.map_or(Value::Null, ns),
        })
    });
    let avg_fill = fill.map(|fill| {
        json!({
            "price": fill.filled_price.to_string(),
            "qty": fill.filled_qty.to_string(),
            "lines": fill.trades.iter().map(|trade| json!({
                "id": trade.trade_id,
                "price": trade.price.to_string(),
                "qty": trade.qty.to_string(),
                "commission": trade.commission.to_string(),
                "commission_asset": trade.commission_asset,
            })).collect::<Vec<_>>(),
        })
    });
    let depth = touch
        .zip(fill)
        .map(|(touch, fill)| depth_walked(side, &touch.snapshot, fill.filled_qty));
    let rate = rates.zip(fill).map(|(rates, fill)| {
        json!({
            "side": word(side),
            "taker": rates.standard.taker.to_string(),
            "applied": effective_rate(side, rates).ok().map(|rate| rate.to_string()),
            "commission_asset": fill.commission_asset,
            "commission": fill.commission.to_string(),
        })
    });
    json!({
        "touch_at_send": touch_at_send,
        "sent_ns": sent.map_or(Value::Null, |sent| ns(sent.sent_ns)),
        "returned_ns": sent.and_then(|sent| sent.returned_ns).map_or(Value::Null, ns),
        "transact_time_ms": fill.and_then(|fill| fill.venue_time_ms),
        "avg_fill": avg_fill,
        "depth_walked": depth,
        "rate": rate,
        "booked_touch": Value::Null,
    })
}

/// The names of the fields every order's `references` has, present or `null`.
pub(crate) const ORDER_REFERENCE_FIELDS: [&str; 8] = [
    "touch_at_send",
    "sent_ns",
    "returned_ns",
    "transact_time_ms",
    "avg_fill",
    "depth_walked",
    "rate",
    "booked_touch",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balance::SpotBalance;
    use crate::cex::{Commission, CommissionDiscount};
    use crate::Provenance;
    use std::str::FromStr;

    fn d(text: &str) -> Decimal {
        Decimal::from_str(text).unwrap()
    }

    fn trade(id: u64, price: &str, qty: &str, commission: &str, asset: &str) -> CexTrade {
        CexTrade {
            trade_id: Some(id),
            price: d(price),
            qty: d(qty),
            commission: d(commission),
            commission_asset: asset.to_string(),
        }
    }

    /// A buy of 9.9 AERO in two trades: 5 at 1.0010 and 4.9 at 1.0020, with the
    /// commission, 0.1 %, in AERO.
    fn buy() -> CexFill {
        let trades = vec![
            trade(11, "1.0010", "5", "0.005", "AERO"),
            trade(12, "1.0020", "4.9", "0.0049", "AERO"),
        ];
        CexFill {
            filled_qty: d("9.9"),
            filled_price: d("1.001495"),
            commission: d("0.0099"),
            commission_asset: "AERO".to_string(),
            provenance: Provenance::Landed,
            order_ref: Some(42),
            client_order_id: Some("vp-1".to_string()),
            venue_time_ms: Some(1_700_000_000_123),
            trades,
        }
    }

    /// A sell of 9.8 AERO in one trade at 1.0000, the commission in USDT.
    fn sell() -> CexFill {
        CexFill {
            filled_qty: d("9.8"),
            filled_price: d("1.0000"),
            commission: d("0.0098"),
            commission_asset: "USDT".to_string(),
            provenance: Provenance::Landed,
            order_ref: Some(43),
            client_order_id: Some("vp-2".to_string()),
            venue_time_ms: Some(1_700_000_001_000),
            trades: vec![trade(21, "1.0000", "9.8", "0.0098", "USDT")],
        }
    }

    fn rules() -> SymbolRules {
        SymbolRules {
            symbol: "AEROUSDT".to_string(),
            status: "TRADING".to_string(),
            base_asset: "AERO".to_string(),
            quote_asset: "USDT".to_string(),
            lot_step: d("0.1"),
            min_qty: d("0.1"),
            min_notional: Some(d("5")),
            apply_min_to_market: Some(true),
        }
    }

    fn block(taker: &str, buyer: &str, seller: &str) -> Commission {
        Commission {
            maker: d(taker),
            taker: d(taker),
            buyer: d(buyer),
            seller: d(seller),
        }
    }

    fn rates() -> SymbolCommission {
        SymbolCommission {
            symbol: "AEROUSDT".to_string(),
            standard: block("0.001", "0", "0"),
            special: block("0", "0", "0"),
            tax: block("0", "0", "0"),
            discount: CommissionDiscount {
                enabled_for_account: false,
                enabled_for_symbol: false,
                asset: None,
                rate: Decimal::ZERO,
            },
        }
    }

    fn line(trade: &CexTrade) -> TradeLine {
        TradeLine {
            price: trade.price,
            qty: trade.qty,
            commission: trade.commission,
            commission_asset: trade.commission_asset.clone(),
            order_id: Some(42),
            trade_id: trade.trade_id,
        }
    }

    fn account(holdings: &[(&str, &str, &str)]) -> SpotAccountBalances {
        SpotAccountBalances {
            balances: holdings
                .iter()
                .map(|(asset, free, locked)| SpotBalance {
                    asset: (*asset).to_string(),
                    free: d(free),
                    locked: d(locked),
                })
                .collect(),
            update_time_ms: None,
        }
    }

    // --- E1 ---

    fn state_of(fill: &CexFill) -> OrderState {
        OrderState::Filled(fill.clone())
    }

    #[test]
    fn e1_a_state_equal_to_the_fill_passes_whatever_order_the_trades_come_in() {
        let fill = buy();
        assert_eq!(e1_order_state(&fill, d("9.9"), &state_of(&fill)), Ok(()));
        let mut reversed = fill.clone();
        reversed.trades.reverse();
        assert_eq!(
            e1_order_state(&fill, d("9.9"), &state_of(&reversed)),
            Ok(())
        );
        // The status query carries its own time, which is not compared.
        let mut later = fill.clone();
        later.venue_time_ms = Some(1_700_000_000_130);
        assert_eq!(e1_order_state(&fill, d("9.9"), &state_of(&later)), Ok(()));
    }

    #[test]
    fn e1_a_part_of_an_order_is_partly_filled_and_the_status_must_say_so() {
        let fill = buy();
        // 9.9 filled of 12 asked for: the venue must say PartlyFilled.
        assert!(e1_order_state(&fill, d("12"), &OrderState::PartlyFilled(fill.clone())).is_ok());
        let why = e1_order_state(&fill, d("12"), &state_of(&fill)).unwrap_err();
        assert!(why.contains("status Filled"), "{why}");
        let why =
            e1_order_state(&fill, d("9.9"), &OrderState::PartlyFilled(fill.clone())).unwrap_err();
        assert!(why.contains("status PartlyFilled"), "{why}");
    }

    /// Every field E1 compares, changed one at a time.
    #[test]
    fn e1_fails_for_each_field_that_differs() {
        let fill = buy();
        type Mutation = (&'static str, fn(&mut CexFill));
        let mutations: [Mutation; 8] = [
            ("executed quantity", |f| f.filled_qty = d("9.8")),
            ("price", |f| f.filled_price = d("1.001496")),
            ("commission", |f| f.commission = d("0.0098")),
            ("commission", |f| f.commission_asset = "BNB".to_string()),
            ("order id", |f| f.order_ref = Some(99)),
            ("client order id", |f| {
                f.client_order_id = Some("vp-x".into())
            }),
            ("provenance", |f| f.provenance = Provenance::Simulated),
            ("trades", |f| f.trades[1].price = d("1.0030")),
        ];
        for (name, mutate) in mutations {
            let mut other = fill.clone();
            mutate(&mut other);
            let why = e1_order_state(&fill, d("9.9"), &state_of(&other)).unwrap_err();
            assert!(why.contains(name), "{name}: {why}");
        }
        let mut dropped = fill.clone();
        dropped.trades.pop();
        let why = e1_order_state(&fill, d("9.9"), &state_of(&dropped)).unwrap_err();
        assert!(
            why.contains("2 trades against 1") || why.contains("1 trades against 2"),
            "{why}"
        );
        for other in [OrderState::Open, OrderState::NotFound] {
            assert!(e1_order_state(&fill, d("9.9"), &other).is_err());
        }
    }

    // --- E2 ---

    #[test]
    fn e2_lines_equal_to_the_trades_pass_in_any_order_and_each_field_matters() {
        let fill = buy();
        let lines: Vec<TradeLine> = fill.trades.iter().map(line).collect();
        assert_eq!(e2_my_trades(&fill, &lines), Ok(()));
        let reversed: Vec<TradeLine> = lines.iter().rev().cloned().collect();
        assert_eq!(e2_my_trades(&fill, &reversed), Ok(()));

        let mutations: [fn(&mut TradeLine); 5] = [
            |l| l.trade_id = Some(99),
            |l| l.price = d("1.0011"),
            |l| l.qty = d("4.8"),
            |l| l.commission = d("0.0050001"),
            |l| l.commission_asset = "USDT".to_string(),
        ];
        for mutate in mutations {
            let mut changed = lines.clone();
            mutate(&mut changed[0]);
            assert!(e2_my_trades(&fill, &changed).is_err());
        }
        assert!(e2_my_trades(&fill, &lines[..1]).is_err(), "a line short");
        let mut extra = lines.clone();
        extra.push(line(&trade(13, "1.0030", "1", "0.001", "AERO")));
        assert!(e2_my_trades(&fill, &extra).is_err(), "a line more");
    }

    // --- E3 ---

    fn public(id: u64, price: &str, qty: &str) -> PublicTrade {
        PublicTrade {
            id,
            price: d(price),
            qty: d(qty),
            time_ms: 1_700_000_000_000,
            is_buyer_maker: false,
        }
    }

    #[test]
    fn e3_trades_found_with_the_same_price_and_quantity_pass() {
        let list = [
            public(10, "1.0", "1"),
            public(11, "1.0010", "5"),
            public(12, "1.0020", "4.9"),
        ];
        assert_eq!(e3_public_trades(&buy(), &list), Grade::Pass);
    }

    #[test]
    fn e3_a_trade_with_another_price_or_quantity_fails_even_when_another_is_missing() {
        let wrong_price = [public(11, "1.0011", "5"), public(12, "1.0020", "4.9")];
        assert!(
            matches!(e3_public_trades(&buy(), &wrong_price), Grade::Fail(why) if why.contains("trade 11"))
        );
        let wrong_qty = [public(11, "1.0010", "5"), public(12, "1.0020", "4.8")];
        assert!(
            matches!(e3_public_trades(&buy(), &wrong_qty), Grade::Fail(why) if why.contains("trade 12"))
        );
        let one_wrong_one_missing = [public(11, "1.0011", "5")];
        assert!(matches!(
            e3_public_trades(&buy(), &one_wrong_one_missing),
            Grade::Fail(_)
        ));
    }

    /// A trade the window no longer holds is a skip that says so, never a pass.
    #[test]
    fn e3_a_trade_the_window_no_longer_holds_is_skipped_window_never_a_pass() {
        let partly = [public(12, "1.0020", "4.9")];
        let Grade::Skip(why) = e3_public_trades(&buy(), &partly) else {
            panic!("expected a skip");
        };
        assert!(why.starts_with("skipped_window"), "{why}");
        assert!(why.contains("[11]") && why.contains("1 of 2"), "{why}");
        assert!(matches!(e3_public_trades(&buy(), &[]), Grade::Skip(_)));
    }

    #[test]
    fn e3_a_fill_with_no_trades_or_a_trade_with_no_id_is_a_failure() {
        let mut none = buy();
        none.trades.clear();
        assert!(matches!(e3_public_trades(&none, &[]), Grade::Fail(_)));
        let mut anonymous = buy();
        anonymous.trades[0].trade_id = None;
        assert!(matches!(e3_public_trades(&anonymous, &[]), Grade::Fail(_)));
    }

    // --- E4 ---

    #[test]
    fn e4_a_buy_pays_its_quantity_times_the_rate_in_the_base_asset() {
        assert_eq!(
            e4_commission(OrderSide::Buy, &buy(), &rules(), &rates()),
            E4::Pass(String::new())
        );
    }

    #[test]
    fn e4_a_sell_pays_its_quote_amount_times_the_rate_in_the_quote_asset() {
        assert_eq!(
            e4_commission(OrderSide::Sell, &sell(), &rules(), &rates()),
            E4::Pass(String::new())
        );
    }

    #[test]
    fn e4_a_commission_off_by_more_than_the_last_decimal_fails() {
        let mut charged_twice = buy();
        charged_twice.trades[0].commission = d("0.010");
        let E4::Fail(why) = e4_commission(OrderSide::Buy, &charged_twice, &rules(), &rates())
        else {
            panic!("expected a failure");
        };
        assert!(
            why.contains("trade Some(11)") && why.contains("0.005"),
            "{why}"
        );
        let mut too_little = sell();
        too_little.trades[0].commission = d("0.0097");
        assert!(matches!(
            e4_commission(OrderSide::Sell, &too_little, &rules(), &rates()),
            E4::Fail(_)
        ));
        // And the rate matters: the same fill against another rate.
        let mut other_rate = rates();
        other_rate.standard = block("0.00075", "0", "0");
        assert!(matches!(
            e4_commission(OrderSide::Buy, &buy(), &rules(), &other_rate),
            E4::Fail(_)
        ));
    }

    /// The last decimal's rounding is not documented: one unit of it passes and
    /// is said; two do not.
    #[test]
    fn e4_one_unit_of_the_last_decimal_passes_and_says_so() {
        let mut fill = sell();
        fill.trades[0] = trade(21, "1.0003", "9.8", "0.00980294", "USDT");
        // 9.8 x 1.0003 x 0.001 = 0.009802940; the venue rounded the other way.
        let E4::Pass(note) = e4_commission(OrderSide::Sell, &fill, &rules(), &rates()) else {
            panic!("expected a pass");
        };
        assert!(note.is_empty(), "{note}");
        fill.trades[0].commission = d("0.00980295");
        let E4::Pass(note) = e4_commission(OrderSide::Sell, &fill, &rules(), &rates()) else {
            panic!("expected a pass");
        };
        assert!(note.contains("last decimal"), "{note}");
        fill.trades[0].commission = d("0.00980296");
        assert!(matches!(
            e4_commission(OrderSide::Sell, &fill, &rules(), &rates()),
            E4::Fail(_)
        ));
    }

    #[test]
    fn e4_a_commission_in_another_asset_is_skipped_asset_with_the_rate_said() {
        let mut in_bnb = buy();
        in_bnb.commission_asset = "BNB".to_string();
        let E4::SkippedAsset(why) = e4_commission(OrderSide::Buy, &in_bnb, &rules(), &rates())
        else {
            panic!("expected a skip");
        };
        assert!(
            why.starts_with("skipped_asset") && why.contains("BNB") && why.contains("0.001"),
            "{why}"
        );
        // A buy paid in the quote asset is not paid in the asset received.
        let mut in_quote = buy();
        in_quote.commission_asset = "USDT".to_string();
        assert!(matches!(
            e4_commission(OrderSide::Buy, &in_quote, &rules(), &rates()),
            E4::SkippedAsset(_)
        ));
    }

    #[test]
    fn the_rate_adds_the_taker_and_the_sides_own_of_each_block() {
        let mut rates = rates();
        rates.standard = block("0.001", "0.0002", "0.0003");
        rates.special = block("0.0001", "0", "0.00005");
        rates.tax = block("0.00001", "0.00002", "0");
        assert_eq!(
            effective_rate(OrderSide::Buy, &rates).unwrap(),
            d("0.00133")
        );
        assert_eq!(
            effective_rate(OrderSide::Sell, &rates).unwrap(),
            d("0.001460")
        );
    }

    // --- E5 ---

    /// 100 USDT and 20 AERO before. The buy takes 5 at 1.0010 and 4.9 at 1.0020,
    /// 9.9148 USDT, and delivers 9.9 AERO less a commission of 0.0099 AERO.
    #[test]
    fn e5_an_account_that_changed_by_exactly_the_fill_passes() {
        let before = account(&[("USDT", "100", "0"), ("AERO", "20", "0")]);
        let after = account(&[("USDT", "90.0852", "0"), ("AERO", "29.8901", "0")]);
        assert_eq!(
            e5_balances(OrderSide::Buy, &rules(), &buy(), &before, &after),
            Ok(())
        );
    }

    #[test]
    fn e5_every_difference_in_the_change_is_a_failure_and_is_named() {
        let before = account(&[("USDT", "100", "0"), ("AERO", "20", "0")]);
        let exact = account(&[("USDT", "90.0852", "0"), ("AERO", "29.8901", "0")]);
        assert_eq!(
            e5_balances(OrderSide::Buy, &rules(), &buy(), &before, &exact),
            Ok(())
        );

        // The quote asset short by a unit.
        let mut short = exact.clone();
        short.balances[0].free = d("90.0851");
        let why = e5_balances(OrderSide::Buy, &rules(), &buy(), &before, &short).unwrap_err();
        assert!(why.contains("USDT changed by -9.9149"), "{why}");
        // The base asset's commission not taken.
        let mut untaxed = exact.clone();
        untaxed.balances[1].free = d("29.9");
        assert!(e5_balances(OrderSide::Buy, &rules(), &buy(), &before, &untaxed).is_err());
        // An asset the fill does not touch moved.
        let mut deposit = exact.clone();
        deposit.balances.push(SpotBalance {
            asset: "BNB".to_string(),
            free: d("1"),
            locked: Decimal::ZERO,
        });
        let why = e5_balances(OrderSide::Buy, &rules(), &buy(), &before, &deposit).unwrap_err();
        assert!(why.contains("BNB changed by 1"), "{why}");
        // What is locked is still held.
        let mut locked = exact.clone();
        locked.balances[0] = SpotBalance {
            asset: "USDT".to_string(),
            free: d("80.0852"),
            locked: d("10"),
        };
        assert_eq!(
            e5_balances(OrderSide::Buy, &rules(), &buy(), &before, &locked),
            Ok(())
        );
        // The sell, the other way.
        let after_sell = account(&[("USDT", "109.7902", "0"), ("AERO", "10.2", "0")]);
        assert_eq!(
            e5_balances(
                OrderSide::Sell,
                &rules(),
                &sell(),
                &account(&[("USDT", "100", "0"), ("AERO", "20", "0")]),
                &after_sell
            ),
            Ok(())
        );
    }

    /// An asset that is absent is held at zero: omitted zero balances.
    #[test]
    fn e5_an_asset_the_account_did_not_list_is_held_at_zero() {
        let before = account(&[("USDT", "100", "0")]);
        let after = account(&[("USDT", "90.0852", "0"), ("AERO", "9.8901", "0")]);
        assert_eq!(
            e5_balances(OrderSide::Buy, &rules(), &buy(), &before, &after),
            Ok(())
        );
        // The account sold out: its AERO is no longer listed.
        let emptied = account(&[("USDT", "109.7902", "0")]);
        let before_sell = account(&[("USDT", "100", "0"), ("AERO", "9.8", "0")]);
        assert_eq!(
            e5_balances(OrderSide::Sell, &rules(), &sell(), &before_sell, &emptied),
            Ok(())
        );
    }

    // --- E6 ---

    #[test]
    fn e6_a_whole_number_of_steps_and_the_venues_own_time_pass() {
        let reply = r#"{"orderId":42,"transactTime":1700000000123}"#;
        assert_eq!(e6_step_and_time(&buy(), d("0.1"), Some(reply)), Ok(()));
    }

    #[test]
    fn e6_each_thing_it_asserts_can_fail_alone() {
        let reply = r#"{"orderId":42,"transactTime":1700000000123}"#;
        let mut off_step = buy();
        off_step.filled_qty = d("9.95");
        let why = e6_step_and_time(&off_step, d("0.1"), Some(reply)).unwrap_err();
        assert!(why.contains("not a whole number of steps"), "{why}");

        let mut other_time = buy();
        other_time.venue_time_ms = Some(1_700_000_000_124);
        let why = e6_step_and_time(&other_time, d("0.1"), Some(reply)).unwrap_err();
        assert!(why.contains("transactTime is 1700000000123"), "{why}");
        let mut no_time = buy();
        no_time.venue_time_ms = None;
        assert!(e6_step_and_time(&no_time, d("0.1"), Some(reply)).is_err());

        let why = e6_step_and_time(&buy(), d("0.1"), Some(r#"{"orderId":42}"#)).unwrap_err();
        assert!(why.contains("no transactTime"), "{why}");
        let why = e6_step_and_time(&buy(), d("0.1"), None).unwrap_err();
        assert!(why.contains("was not seen"), "{why}");
        let why = e6_step_and_time(&buy(), d("0.1"), Some("not json")).unwrap_err();
        assert!(why.contains("no transactTime"), "{why}");
    }

    // --- the references ---

    fn snapshot() -> OrderBookSnapshot {
        OrderBookSnapshot {
            last_update_id: 1_027_024,
            bids: vec![(d("1.0000"), d("5")), (d("0.9990"), d("300"))],
            asks: vec![(d("1.0010"), d("5")), (d("1.0020"), d("300"))],
        }
    }

    fn touch() -> Touch {
        Touch {
            snapshot: snapshot(),
            read_sent_ns: 1_000,
            read_returned_ns: Some(2_000),
        }
    }

    fn exchange() -> Exchange {
        Exchange {
            method: "POST".to_string(),
            path: "/api/v3/order".to_string(),
            sent_ns: 5_000,
            returned_ns: Some(9_000),
            status: Some(200),
            body: Some("{}".to_string()),
        }
    }

    #[test]
    fn the_references_of_a_fill_name_every_field_of_the_table() {
        let references = order_references(
            OrderSide::Buy,
            Some(&touch()),
            Some(&exchange()),
            Some(&buy()),
            Some(&rates()),
        );
        for field in ORDER_REFERENCE_FIELDS {
            assert!(references.get(field).is_some(), "{field} is absent");
        }
        assert_eq!(references["touch_at_send"]["best_bid"]["price"], "1.0000");
        assert_eq!(references["touch_at_send"]["best_ask"]["qty"], "5");
        assert_eq!(references["touch_at_send"]["last_update_id"], 1_027_024);
        assert_eq!(references["touch_at_send"]["read_returned_ns"], 2_000);
        assert_eq!(
            (&references["sent_ns"], &references["returned_ns"]),
            (&json!(5_000), &json!(9_000))
        );
        assert_eq!(references["transact_time_ms"], 1_700_000_000_123u64);
        assert_eq!(references["avg_fill"]["price"], "1.001495");
        assert_eq!(references["avg_fill"]["lines"][1]["id"], 12);
        assert_eq!(
            references["avg_fill"]["lines"][1]["commission_asset"],
            "AERO"
        );
        assert_eq!(references["rate"]["side"], "buy");
        assert_eq!(references["rate"]["taker"], "0.001");
        assert_eq!(references["rate"]["commission_asset"], "AERO");
        assert!(
            references["booked_touch"].is_null(),
            "term 4 has no decision to book"
        );
    }

    /// 9.9 bought walks the asks: 5 at the first level and 4.9 at the second.
    #[test]
    fn the_depth_walked_is_the_levels_the_quantity_would_have_used() {
        let references = order_references(OrderSide::Buy, Some(&touch()), None, Some(&buy()), None);
        let depth = &references["depth_walked"];
        assert_eq!(depth["side"], "asks");
        assert_eq!(
            depth["levels"][0],
            json!({"price": "1.0010", "level_qty": "5", "used_qty": "5"})
        );
        assert_eq!(
            depth["levels"][1],
            json!({"price": "1.0020", "level_qty": "300", "used_qty": "4.9"})
        );
        assert_eq!(depth["beyond_snapshot_qty"], "0");

        // A sell takes the bids; a quantity the snapshot cannot hold says how much.
        let mut deep = sell();
        deep.filled_qty = d("400");
        let references = order_references(OrderSide::Sell, Some(&touch()), None, Some(&deep), None);
        let depth = &references["depth_walked"];
        assert_eq!(depth["side"], "bids");
        assert_eq!(depth["levels"].as_array().unwrap().len(), 2);
        assert_eq!(depth["beyond_snapshot_qty"], "95");
    }

    /// A reference that does not exist is null, never zero or absent.
    #[test]
    fn a_reference_that_does_not_exist_is_null_never_zero_or_absent() {
        let nothing = order_references(OrderSide::Buy, None, None, None, None);
        for field in ORDER_REFERENCE_FIELDS {
            assert!(nothing[field].is_null(), "{field}: {}", nothing[field]);
        }
        // The book was read and the order was refused: the touch exists, the fill does not.
        let refused = order_references(
            OrderSide::Buy,
            Some(&touch()),
            Some(&exchange()),
            None,
            Some(&rates()),
        );
        assert!(!refused["touch_at_send"].is_null());
        for field in [
            "transact_time_ms",
            "avg_fill",
            "depth_walked",
            "rate",
            "booked_touch",
        ] {
            assert!(refused[field].is_null(), "{field}");
        }
        // A request that got no reply has a departure and no return.
        let mut lost = exchange();
        lost.returned_ns = None;
        let references = order_references(OrderSide::Buy, None, Some(&lost), None, None);
        assert_eq!(references["sent_ns"], 5_000);
        assert!(references["returned_ns"].is_null());
    }
}
