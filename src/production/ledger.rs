//! The spend ledger: what a production run may lose, and the check that comes
//! before an order is signed.
//!
//! Two caps, both in USD and both `Decimal`:
//!
//! - **The order cap** (`VP_ORDER_CAP_USD`, default 12): no order's notional may
//!   be above it. Checked before the order is signed.
//! - **The spend cap** (`VP_SPEND_CAP_USD`, required): the run's cumulative loss,
//!   valued at the fills' own prices. An order is refused when the loss is
//!   already at the cap, and when the loss plus *this order's whole notional*
//!   would pass it: a market order can at worst lose what it spends, so the
//!   loss of a run can never pass the cap by an order that was allowed. A cap
//!   set below an order plus the run's losses so far stops the run early; that
//!   is the cap doing its work.
//!
//! The ledger values what it can: the quote asset of a symbol must be one of
//! the USD stablecoins it is told about, and a commission is counted in the
//! quote or the base asset. A fill, a commission or a quote asset it cannot
//! value **stops the run**: it is an error, never a zero, and every later
//! order is refused.
//!
//! Reads are not orders. A run that has reached its cap can still read its
//! balances to say what it ended with.

use rust_decimal::Decimal;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Mutex;

/// The quote assets valued at one dollar each.
const USD_ASSETS: [&str; 6] = ["USDT", "USDC", "FDUSD", "BUSD", "TUSD", "USD"];

/// Why the ledger refused an order, or stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The order's notional is above the order cap.
    OrderOverCap { notional: Decimal, cap: Decimal },
    /// The loss so far is at the spend cap.
    SpendCapReached { loss: Decimal, cap: Decimal },
    /// The loss so far plus the order's notional would pass the spend cap.
    WouldPassSpendCap {
        loss: Decimal,
        notional: Decimal,
        cap: Decimal,
    },
    /// A figure the ledger could not compute. It stops the run, and every order
    /// after it is refused.
    Uncomputable(String),
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stop::OrderOverCap { notional, cap } => write!(
                f,
                "an order of {notional} USD is over the order cap of {cap} USD"
            ),
            Stop::SpendCapReached { loss, cap } => {
                write!(f, "the run has lost {loss} USD, at its cap of {cap} USD")
            }
            Stop::WouldPassSpendCap {
                loss,
                notional,
                cap,
            } => write!(
                f,
                "the run has lost {loss} USD and this order spends {notional} USD: \
                 it could pass the cap of {cap} USD"
            ),
            Stop::Uncomputable(why) => {
                write!(
                    f,
                    "the ledger cannot compute a figure, so the run stops: {why}"
                )
            }
        }
    }
}

impl std::error::Error for Stop {}

/// A fill, as the ledger values it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fill<'a> {
    pub(crate) base_asset: &'a str,
    pub(crate) quote_asset: &'a str,
    pub(crate) buy: bool,
    pub(crate) qty: Decimal,
    pub(crate) price: Decimal,
    pub(crate) commission: Decimal,
    pub(crate) commission_asset: &'a str,
}

#[derive(Default)]
struct State {
    /// Quote currency spent (negative) and received (positive), in USD.
    cash: Decimal,
    /// The base assets held, each with the price of the last fill that moved it.
    holdings: BTreeMap<String, (Decimal, Decimal)>,
    /// Losses recorded outright, in USD (gas, a swap's shortfall).
    recorded: Decimal,
    /// Set by the first figure that could not be computed.
    stopped: Option<String>,
}

pub(crate) struct Ledger {
    spend_cap: Decimal,
    order_cap: Decimal,
    usd: BTreeSet<&'static str>,
    state: Mutex<State>,
}

impl Ledger {
    pub(crate) fn new(spend_cap_usd: Decimal, order_cap_usd: Decimal) -> Self {
        Self {
            spend_cap: spend_cap_usd,
            order_cap: order_cap_usd,
            usd: USD_ASSETS.into_iter().collect(),
            state: Mutex::new(State::default()),
        }
    }

    pub(crate) fn spend_cap(&self) -> Decimal {
        self.spend_cap
    }

    pub(crate) fn order_cap(&self) -> Decimal {
        self.order_cap
    }

    /// The cumulative loss so far, in USD: what was spent less what was
    /// received, less the base assets held at their last fill price, plus what
    /// was recorded outright. Negative is a gain.
    pub(crate) fn loss(&self) -> Result<Decimal, Stop> {
        let state = self.state.lock().unwrap();
        if let Some(why) = &state.stopped {
            return Err(Stop::Uncomputable(why.clone()));
        }
        loss_of(&state)
    }

    /// Whether an order of `notional_usd` may be signed.
    pub(crate) fn authorise(&self, notional_usd: Decimal) -> Result<(), Stop> {
        let state = self.state.lock().unwrap();
        if let Some(why) = &state.stopped {
            return Err(Stop::Uncomputable(why.clone()));
        }
        if notional_usd <= Decimal::ZERO {
            return Err(Stop::Uncomputable(format!(
                "an order's notional is {notional_usd}: it must be greater than zero"
            )));
        }
        if notional_usd > self.order_cap {
            return Err(Stop::OrderOverCap {
                notional: notional_usd,
                cap: self.order_cap,
            });
        }
        let loss = loss_of(&state)?;
        if loss >= self.spend_cap {
            return Err(Stop::SpendCapReached {
                loss,
                cap: self.spend_cap,
            });
        }
        let at_risk = loss
            .checked_add(notional_usd)
            .ok_or_else(|| Stop::Uncomputable("loss plus notional overflows".to_string()))?;
        if at_risk > self.spend_cap {
            return Err(Stop::WouldPassSpendCap {
                loss,
                notional: notional_usd,
                cap: self.spend_cap,
            });
        }
        Ok(())
    }

    /// Books a fill at its own price. A fill the ledger cannot value stops the
    /// run (and is returned as the error).
    pub(crate) fn record_fill(&self, fill: &Fill<'_>) -> Result<Decimal, Stop> {
        let mut state = self.state.lock().unwrap();
        if let Some(why) = &state.stopped {
            return Err(Stop::Uncomputable(why.clone()));
        }
        match self.apply(&mut state, fill) {
            Ok(()) => loss_of(&state),
            Err(why) => {
                state.stopped = Some(why.clone());
                Err(Stop::Uncomputable(why))
            }
        }
    }

    /// Books a loss outright, in USD (a negative amount is a gain).
    pub(crate) fn record_loss(&self, amount_usd: Decimal) -> Result<Decimal, Stop> {
        let mut state = self.state.lock().unwrap();
        if let Some(why) = &state.stopped {
            return Err(Stop::Uncomputable(why.clone()));
        }
        match state.recorded.checked_add(amount_usd) {
            Some(total) => state.recorded = total,
            None => {
                let why = "a recorded loss overflows".to_string();
                state.stopped = Some(why.clone());
                return Err(Stop::Uncomputable(why));
            }
        }
        loss_of(&state)
    }

    /// Stops the run because a figure could not be had. Every later order is
    /// refused.
    pub(crate) fn stop(&self, why: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        state.stopped.get_or_insert(why.into());
    }

    fn apply(&self, state: &mut State, fill: &Fill<'_>) -> Result<(), String> {
        if !self.usd.contains(fill.quote_asset) {
            return Err(format!(
                "the quote asset {} is not one the ledger values in USD",
                fill.quote_asset
            ));
        }
        if fill.qty <= Decimal::ZERO || fill.price <= Decimal::ZERO {
            return Err(format!(
                "a fill of {} at {} cannot be valued",
                fill.qty, fill.price
            ));
        }
        let overflow = || "a fill's value overflows".to_string();
        let value = fill.qty.checked_mul(fill.price).ok_or_else(overflow)?;
        let (cash_delta, base_delta) = if fill.buy {
            (-value, fill.qty)
        } else {
            (value, -fill.qty)
        };
        let mut cash = state.cash.checked_add(cash_delta).ok_or_else(overflow)?;
        let mut base = state
            .holdings
            .get(fill.base_asset)
            .map_or(Decimal::ZERO, |(qty, _)| *qty)
            .checked_add(base_delta)
            .ok_or_else(overflow)?;
        if fill.commission < Decimal::ZERO {
            return Err(format!(
                "a commission of {} cannot be valued",
                fill.commission
            ));
        }
        if fill.commission_asset == fill.quote_asset {
            cash = cash.checked_sub(fill.commission).ok_or_else(overflow)?;
        } else if fill.commission_asset == fill.base_asset {
            base = base.checked_sub(fill.commission).ok_or_else(overflow)?;
        } else if !fill.commission.is_zero() {
            return Err(format!(
                "a commission of {} {} has no price in USD here",
                fill.commission, fill.commission_asset
            ));
        }
        state.cash = cash;
        state
            .holdings
            .insert(fill.base_asset.to_string(), (base, fill.price));
        Ok(())
    }
}

fn loss_of(state: &State) -> Result<Decimal, Stop> {
    let overflow = || Stop::Uncomputable("the loss overflows".to_string());
    let mut value = state.cash;
    for (qty, price) in state.holdings.values() {
        let held = qty.checked_mul(*price).ok_or_else(overflow)?;
        value = value.checked_add(held).ok_or_else(overflow)?;
    }
    state.recorded.checked_sub(value).ok_or_else(overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(text: &str) -> Decimal {
        Decimal::from_str(text).unwrap()
    }

    fn fill<'a>(
        buy: bool,
        qty: &str,
        price: &str,
        commission: &str,
        commission_asset: &'a str,
    ) -> Fill<'a> {
        Fill {
            base_asset: "AERO",
            quote_asset: "USDT",
            buy,
            qty: d(qty),
            price: d(price),
            commission: d(commission),
            commission_asset,
        }
    }

    #[test]
    fn an_order_over_the_order_cap_is_refused_before_anything_is_spent() {
        let ledger = Ledger::new(d("50"), d("12"));
        assert_eq!(ledger.authorise(d("12")), Ok(()));
        assert_eq!(
            ledger.authorise(d("12.01")),
            Err(Stop::OrderOverCap {
                notional: d("12.01"),
                cap: d("12")
            })
        );
    }

    #[test]
    fn an_order_that_could_pass_the_spend_cap_is_refused() {
        let ledger = Ledger::new(d("15"), d("12"));
        // A round trip that costs 0.30: bought at 1.00, sold at 0.97.
        ledger
            .record_fill(&fill(true, "10", "1.00", "0", "AERO"))
            .unwrap();
        let loss = ledger
            .record_fill(&fill(false, "10", "0.97", "0", "USDT"))
            .unwrap();
        assert_eq!(loss, d("0.30"));
        // 0.30 lost; an order of 12 could take it to 12.30: allowed.
        assert_eq!(ledger.authorise(d("12")), Ok(()));
        // Another 3 lost brings it to 3.30, and an order of 12 could reach 15.30.
        ledger.record_loss(d("3")).unwrap();
        assert_eq!(
            ledger.authorise(d("12")),
            Err(Stop::WouldPassSpendCap {
                loss: d("3.30"),
                notional: d("12"),
                cap: d("15")
            })
        );
        // A smaller order still fits.
        assert_eq!(ledger.authorise(d("11.70")), Ok(()));
    }

    #[test]
    fn a_run_at_its_cap_places_nothing_more() {
        let ledger = Ledger::new(d("5"), d("12"));
        ledger.record_loss(d("5")).unwrap();
        assert_eq!(
            ledger.authorise(d("1")),
            Err(Stop::SpendCapReached {
                loss: d("5"),
                cap: d("5")
            })
        );
    }

    /// The loss is valued at the fills' own prices: what is held is worth the
    /// price it was last bought or sold at.
    #[test]
    fn what_is_held_is_valued_at_the_last_fill_price() {
        let ledger = Ledger::new(d("50"), d("12"));
        // Bought 10 AERO at 1.00 for 10 USDT, paying 0.01 AERO commission: the
        // 9.99 held are worth 9.99, so 0.01 is lost.
        let loss = ledger
            .record_fill(&fill(true, "10", "1.00", "0.01", "AERO"))
            .unwrap();
        assert_eq!(loss, d("0.01"));
        // Sold 9.99 at 1.01 for 10.0899, paying 0.01 USDT: a gain.
        let loss = ledger
            .record_fill(&fill(false, "9.99", "1.01", "0.01", "USDT"))
            .unwrap();
        assert_eq!(loss, d("-0.0799"));
    }

    #[test]
    fn a_commission_in_an_asset_with_no_price_stops_the_run_and_is_not_a_zero() {
        let ledger = Ledger::new(d("50"), d("12"));
        let err = ledger
            .record_fill(&fill(true, "10", "1.00", "0.0003", "BNB"))
            .unwrap_err();
        assert!(matches!(err, Stop::Uncomputable(_)), "{err}");
        assert!(err.to_string().contains("BNB"), "{err}");
        // Every later order, and the loss itself, say so.
        assert!(matches!(
            ledger.authorise(d("1")),
            Err(Stop::Uncomputable(_))
        ));
        assert!(matches!(ledger.loss(), Err(Stop::Uncomputable(_))));
        assert!(matches!(
            ledger.record_loss(d("1")),
            Err(Stop::Uncomputable(_))
        ));
        // A zero commission in any asset is zero, not unknown.
        let ledger = Ledger::new(d("50"), d("12"));
        assert!(ledger
            .record_fill(&fill(true, "10", "1.00", "0", "BNB"))
            .is_ok());
    }

    #[test]
    fn a_quote_asset_that_is_not_a_dollar_stops_the_run() {
        let ledger = Ledger::new(d("50"), d("12"));
        let mut btc_quoted = fill(true, "1", "0.0002", "0", "AERO");
        btc_quoted.quote_asset = "BTC";
        let err = ledger.record_fill(&btc_quoted).unwrap_err();
        assert!(err.to_string().contains("BTC"), "{err}");
        assert!(ledger.authorise(d("1")).is_err());
    }

    #[test]
    fn a_figure_that_cannot_be_had_stops_every_order() {
        let ledger = Ledger::new(d("50"), d("12"));
        ledger.stop("the pool's price could not be read");
        let err = ledger.authorise(d("1")).unwrap_err();
        assert!(err.to_string().contains("pool's price"), "{err}");
    }

    #[test]
    fn nothing_is_authorised_for_a_notional_that_is_not_positive() {
        let ledger = Ledger::new(d("50"), d("12"));
        assert!(matches!(
            ledger.authorise(d("0")),
            Err(Stop::Uncomputable(_))
        ));
        assert!(matches!(
            ledger.authorise(d("-1")),
            Err(Stop::Uncomputable(_))
        ));
    }

    #[test]
    fn a_value_too_large_to_hold_is_an_error_and_not_a_wrap() {
        let ledger = Ledger::new(d("50"), d("12"));
        let huge = Decimal::MAX.to_string();
        let err = ledger
            .record_fill(&fill(true, &huge, "2", "0", "AERO"))
            .unwrap_err();
        assert!(matches!(err, Stop::Uncomputable(_)), "{err}");
    }
}
