//! LV2a against Binance spot (`specs/V7-production-validation.md`, "LV2a"): one
//! exchange order at a time, reconciled to the unit. One test,
//! `production_lv2a_exchange`, makes ten round trips on `VP_SYMBOL`, each a buy and
//! then a sell of what the buy delivered, of about `VP_ORDER_USD`: the first at about
//! 6 USD, the second with a quantity that needs rounding to the lot step.
//!
//! Each leg is a market order unless `VP_BUY_ORDER` or `VP_SELL_ORDER` says
//! `limit-ioc`: a limit order at the touch the case read just before it
//! (`OrderStyle::LimitIoc`), which fills at that price or better and never rests. A
//! limit-ioc leg is refused (nothing sent) when the touch holds less than its
//! quantity, because it would fill in part. The style of each order is on its line,
//! in `references.order_style`, with `references.limit_price`.
//!
//! Before each order the case reads the book (five levels) and the account's
//! balances; it sends the order; and it checks what the venue says of it against
//! what `execute` returned, to the unit (`exchange_checks`):
//!
//! | # | Check | Where |
//! |---|---|---|
//! | E6 | whole lot steps; the fill's time is the venue's `transactTime` | the order's reply, as the gate saw it |
//! | E1 | `order_state` equals the fill | `GET /api/v3/order` and `myTrades` |
//! | E2 | the `myTrades` lines equal the fill's trades | `GET /api/v3/myTrades` |
//! | E3 | each trade is among the public trades | `GET /api/v3/trades?limit=1000` |
//! | E5 | the account changed by exactly the fill | `GET /api/v3/account` |
//! | E4 | the commission is the amount received times the rate | `account_commission`, read once |
//!
//! Any failure stops the run at once, holding whatever the run holds: the line says
//! what. A trade the public list no longer holds is `skipped_window` and a
//! commission paid in another asset is `skipped_asset`, never a pass.
//!
//! **Every order goes through the ledger before it is signed**, and every fill is
//! booked in it at its own price. A commission the ledger cannot value in USD (BNB:
//! switch "pay fees with BNB" off for the account this runs on) stops the run.
//!
//! **What is recorded and not asserted** is on each order's line, in `references`
//! (`exchange_checks::order_references`): the touch at send, the local clock before
//! the signed order left and when its answer came back, the venue's own time, the
//! average fill and each trade, the depth the quantity walked, the rate.
//!
//! A dry run makes no read, so an order, which is sized from one, is skipped with
//! that reason; the reads that start the run are printed.

use crate::balance::{SpotAccountBalances, SpotBalanceReader};
use crate::cex::binance::order::trade_lines;
use crate::cex::{
    BinanceLive, BinanceRest, CexExecutor, CexFill, CexOrders, CexTimings, OrderRequest, OrderSide,
    OrderStyle, SymbolCommission, SymbolRules,
};
use crate::production::env::{positive_count, positive_decimal, Env};
use crate::production::exchange_checks::{
    e1_order_state, e2_my_trades, e3_public_trades, e4_commission, e5_balances, e6_step_and_time,
    effective_rate, order_references, Touch, E4,
};
use crate::production::gate::Exchange;
use crate::production::ledger::{Fill, Stop};
use crate::production::lv1_binance::Flow;
use crate::production::results::{Expected, Tally, Verdict};
use crate::production::sizing::{quantity_for, round_down};
use crate::production::{CallSpec, Grade, Run};
use anyhow::{anyhow, bail, Result};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::str::FromStr;

const EXCHANGE_INFO_PATH: &str = "/api/v3/exchangeInfo";
const ORDER_PATH: &str = "/api/v3/order";
const DEPTH_PATH: &str = "/api/v3/depth";
const MY_TRADES_PATH: &str = "/api/v3/myTrades";

/// The case id of the orders and of the reads around them.
const CASE: &str = "LV2a";
/// How many round trips a run makes.
pub(crate) const ROUND_TRIPS: usize = 10;
/// The size of the first round trip, in USD, when the symbol's minimum allows.
const SMALL_USD: i64 = 6;
/// How far above the symbol's minimum notional the first round trip is sized,
/// so that what a buy delivers can be sold back.
const MINIMUM_HEADROOM: (i64, u32) = (12, 1);

/// What the run is pointed at, from the environment.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// `VP_SYMBOL`, default `AEROUSDT`.
    pub(crate) symbol: String,
    /// `VP_ORDER_USD`, default 10: about what one order is worth.
    pub(crate) order_usd: Decimal,
    /// `VP_ROUND_TRIPS`, default ten: fewer for a pilot.
    pub(crate) round_trips: usize,
    /// `VP_BUY_ORDER`, default `market`: how every buy is expressed.
    pub(crate) buy_order: LegStyle,
    /// `VP_SELL_ORDER`, default `market`: how every sell is expressed.
    pub(crate) sell_order: LegStyle,
}

impl Settings {
    pub(crate) fn from_env(env: &dyn Env) -> Result<Self> {
        Ok(Self {
            symbol: env
                .var("VP_SYMBOL")
                .unwrap_or_else(|| "AEROUSDT".to_string()),
            order_usd: positive_decimal(env, "VP_ORDER_USD", Some(Decimal::from(10)))?,
            round_trips: positive_count(env, "VP_ROUND_TRIPS", ROUND_TRIPS)?,
            buy_order: LegStyle::from_env(env, "VP_BUY_ORDER")?,
            sell_order: LegStyle::from_env(env, "VP_SELL_ORDER")?,
        })
    }
}

/// How a leg's order is expressed to the exchange: what the case asks of the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegStyle {
    /// A market order: no price, so whatever the book offers.
    Market,
    /// A limit order at the touch the case read just before it, immediate-or-cancel
    /// (`OrderStyle::LimitIoc`): it fills at that price or better, or not at all.
    LimitIoc,
}

impl LegStyle {
    /// The word the environment and the recorded line use.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Market => "market",
            Self::LimitIoc => "limit-ioc",
        }
    }

    fn from_env(env: &dyn Env, key: &str) -> Result<Self> {
        match env.var(key).as_deref() {
            None => Ok(Self::Market),
            Some("market") => Ok(Self::Market),
            Some("limit-ioc") => Ok(Self::LimitIoc),
            Some(other) => bail!("{key} is {other:?}: it is `market` or `limit-ioc`"),
        }
    }
}

/// How much a leg is to trade.
#[derive(Debug, Clone, Copy)]
enum Want {
    /// About this much in USD; with `off_step` the quantity asked for is half a
    /// lot step above a whole number of them, so that the adapter has to round it.
    Usd { usd: Decimal, off_step: bool },
    /// This much of the base asset, as the buy delivered it.
    Quantity(Decimal),
}

/// What a leg came to.
enum Leg {
    /// The order filled and every check passed or was skipped, saying why.
    Done(CexFill),
    /// No order was sent, for a reason the line says; the run goes on.
    Skipped,
    /// The run stops here. The fill is the order's, when one filled in this
    /// leg: the account's holdings have changed by it.
    Stopped(Option<CexFill>),
}

pub(crate) struct Lv2aExchange<'a> {
    run: &'a Run,
    env: &'a dyn Env,
    allowed_host: String,
    timings: CexTimings,
    settings: Settings,
    rules: Option<SymbolRules>,
    commission: Option<SymbolCommission>,
    start: Option<SpotAccountBalances>,
    /// The adapter that sends market orders.
    live: Option<BinanceLive>,
    /// The adapter that sends limit-ioc orders, built only when a leg asks for them.
    live_limit: Option<BinanceLive>,
}

fn stem(trip: usize, side: OrderSide) -> String {
    format!(
        "{:02}{}",
        trip + 1,
        match side {
            OrderSide::Buy => "b",
            OrderSide::Sell => "s",
        }
    )
}

fn side_word(side: OrderSide) -> &'static str {
    match side {
        OrderSide::Buy => "BUY",
        OrderSide::Sell => "SELL",
    }
}

/// Whether `rules` say the minimum notional applies to a market order.
fn minimum_notional(rules: &SymbolRules) -> Option<Decimal> {
    rules
        .min_notional
        .filter(|_| rules.apply_min_to_market != Some(false))
}

/// The minimum notional an order of this style must meet: a market order's is
/// the venue's `applyMinToMarket` rule; a limit order always meets it.
fn minimum_for(style: LegStyle, rules: &SymbolRules) -> Option<Decimal> {
    match style {
        LegStyle::Market => minimum_notional(rules),
        LegStyle::LimitIoc => rules.min_notional,
    }
}

/// `PRICE_FILTER.tickSize` of `symbol` in the body of `GET /api/v3/exchangeInfo`, read
/// from the body as the venue sent it (`SymbolRules` does not carry it). `None` where
/// the symbol, the filter or a non-zero tick is not there.
fn price_tick(body: &str, symbol: &str) -> Option<Decimal> {
    let info: Value = serde_json::from_str(body).ok()?;
    let row = info["symbols"]
        .as_array()?
        .iter()
        .find(|row| row["symbol"] == symbol)?;
    let tick = row["filters"]
        .as_array()?
        .iter()
        .find(|filter| filter["filterType"] == "PRICE_FILTER")?["tickSize"]
        .as_str()?;
    Decimal::from_str(tick).ok().filter(|tick| !tick.is_zero())
}

impl<'a> Lv2aExchange<'a> {
    pub(crate) fn new(
        run: &'a Run,
        env: &'a dyn Env,
        allowed_host: String,
        timings: CexTimings,
        settings: Settings,
    ) -> Self {
        Self {
            run,
            env,
            allowed_host,
            timings,
            settings,
            rules: None,
            commission: None,
            start: None,
            live: None,
            live_limit: None,
        }
    }

    fn rest(&self) -> Result<BinanceRest> {
        self.run.binance_rest(
            self.env,
            &self.allowed_host,
            "BINANCE",
            self.timings.clone(),
        )
    }

    fn live(&self) -> Result<&BinanceLive> {
        self.live
            .as_ref()
            .ok_or_else(|| anyhow!("the case went on without having read the symbol's rules"))
    }

    /// The adapter that expresses an order in `style`.
    fn live_for(&self, style: LegStyle) -> Result<&BinanceLive> {
        match style {
            LegStyle::Market => self.live(),
            LegStyle::LimitIoc => self.live_limit.as_ref().ok_or_else(|| {
                anyhow!("a limit-ioc leg was reached without the limit adapter having been built")
            }),
        }
    }

    /// `PRICE_FILTER.tickSize` of `symbol`, from the exchangeInfo the case read first.
    fn price_tick(&self, symbol: &str) -> Option<Decimal> {
        let body = self
            .run
            .gate()
            .exchanges()
            .into_iter()
            .rev()
            .find(|exchange| exchange.path == EXCHANGE_INFO_PATH)
            .and_then(|exchange| exchange.body)?;
        price_tick(&body, symbol)
    }

    fn rules(&self) -> Result<&SymbolRules> {
        self.rules
            .as_ref()
            .ok_or_else(|| anyhow!("the case went on without having read the symbol's rules"))
    }

    /// The round trips, in order. Stops at once when a check fails, the run is
    /// halted or the ledger says no.
    pub(crate) async fn run_all(&mut self) -> Result<Tally> {
        if self.setup().await? == Flow::Stop {
            return Ok(self.run.tally());
        }
        let mut stopped = false;
        for trip in 0..self.settings.round_trips {
            if self.round_trip(trip).await? == Flow::Stop {
                stopped = true;
                break;
            }
        }
        if !stopped {
            self.exit().await?;
        }
        Ok(self.run.tally())
    }

    // --- the reads that start the run ---

    /// The symbol's rules (`status == "TRADING"` is asserted), the commission the
    /// account pays on it, and the account's balances, which the exit compares the
    /// last read against.
    async fn setup(&mut self) -> Result<Flow> {
        let symbol = self.settings.symbol.clone();
        let rest = self.rest()?;
        let rules = self
            .run
            .call_with(
                CallSpec::new(CASE, Expected::ok()).record_as("00-exchange-info"),
                || async { rest.symbol_rules(&symbol).await },
                |_| None,
                |result| match result {
                    Ok(rules) if rules.status == "TRADING" => Ok(()),
                    Ok(rules) => Err(format!(
                        "{symbol}'s status is {:?}, not \"TRADING\"",
                        rules.status
                    )),
                    Err(_) => Ok(()),
                },
            )
            .await?;
        // The tick is in the exchangeInfo body just read: the gate forgets a call's
        // exchanges when the next one begins, so it is taken before the next read.
        let tick = self.price_tick(&symbol);
        let commission = self
            .run
            .call(
                CallSpec::new(CASE, Expected::ok()).record_as("02-account-commission"),
                || async { rest.account_commission(&symbol).await },
            )
            .await?;
        let balances = self
            .run
            .call(
                CallSpec::new(CASE, Expected::ok()).record_as("01-account"),
                || async { rest.balances().await },
            )
            .await?;
        let failed = [rules.verdict, commission.verdict, balances.verdict]
            .iter()
            .any(|verdict| matches!(verdict, Verdict::Fail | Verdict::Halted | Verdict::Unmapped));
        self.rules = rules.result.ok();
        self.commission = commission.result.ok();
        self.start = balances.result.ok();
        if failed {
            return Ok(Flow::Stop);
        }
        let Some(rules) = self.rules.clone() else {
            self.run.note(
                CASE,
                Expected::ok(),
                Verdict::Skipped,
                "the symbol's rules were not read, so no order can be sized",
            )?;
            return Ok(Flow::Stop);
        };
        if self.commission.is_none() || self.start.is_none() {
            self.run.note(
                CASE,
                Expected::ok(),
                Verdict::Skipped,
                "the account's commission or balances were not read, so no order can be checked",
            )?;
            return Ok(Flow::Stop);
        }
        let steps = HashMap::from([(symbol.clone(), rules.lot_step)]);
        self.live = Some(BinanceLive::new(self.rest()?, steps.clone()));
        if [self.settings.buy_order, self.settings.sell_order].contains(&LegStyle::LimitIoc) {
            let Some(tick) = tick else {
                self.run.computed(
                    CASE,
                    Verdict::Fail,
                    &format!(
                        "a limit-ioc leg was asked for and exchangeInfo gives {symbol} no \
                         PRICE_FILTER tick to price it on: nothing was sent"
                    ),
                    None,
                )?;
                return Ok(Flow::Stop);
            };
            self.live_limit = Some(BinanceLive::new(self.rest()?, steps).with_order_style(
                OrderStyle::LimitIoc {
                    tick_size: HashMap::from([(symbol, tick)]),
                },
            ));
        }
        Ok(Flow::Go)
    }

    // --- a round trip ---

    /// The size of trip `trip`: the first is about 6 USD (more, where the symbol's
    /// minimum notional needs it), the second asks for a quantity that needs
    /// rounding to the lot step, the rest are the run's order size.
    fn want(&self, trip: usize, rules: &SymbolRules) -> Want {
        let order_usd = self.settings.order_usd;
        let usd = if trip == 0 {
            let floor = minimum_notional(rules)
                .and_then(|minimum| {
                    minimum.checked_mul(Decimal::new(MINIMUM_HEADROOM.0, MINIMUM_HEADROOM.1))
                })
                .map_or(Decimal::ZERO, |floor| floor.ceil());
            let small = Decimal::from(SMALL_USD).max(floor);
            if small > order_usd {
                order_usd
            } else {
                small
            }
        } else {
            order_usd
        };
        Want::Usd {
            usd,
            off_step: trip == 1,
        }
    }

    async fn round_trip(&self, trip: usize) -> Result<Flow> {
        let rules = self.rules()?.clone();
        let want = self.want(trip, &rules);
        let bought = match self.leg(trip, OrderSide::Buy, want).await? {
            Leg::Done(fill) => fill,
            Leg::Skipped => return Ok(Flow::Go),
            Leg::Stopped(held) => {
                if let Some(fill) = held {
                    self.run.computed(
                        "EXIT",
                        Verdict::Skipped,
                        &format!(
                            "round trip {} stopped after its buy filled: the account holds the {} {} \
                             it bought",
                            trip + 1,
                            fill.filled_qty,
                            rules.base_asset
                        ),
                        None,
                    )?;
                }
                return Ok(Flow::Stop);
            }
        };
        // What the buy delivered: its quantity, less the commission where that
        // was taken from it.
        let delivered = if bought.commission_asset == rules.base_asset {
            bought.filled_qty - bought.commission
        } else {
            bought.filled_qty
        };
        match self
            .leg(trip, OrderSide::Sell, Want::Quantity(delivered))
            .await?
        {
            Leg::Done(_) => Ok(Flow::Go),
            Leg::Skipped | Leg::Stopped(None) => {
                self.run.computed(
                    "EXIT",
                    Verdict::Skipped,
                    &format!(
                        "round trip {} did not finish: the account holds the {} {} its buy delivered",
                        trip + 1,
                        delivered,
                        rules.base_asset
                    ),
                    None,
                )?;
                Ok(Flow::Stop)
            }
            Leg::Stopped(Some(_)) => Ok(Flow::Stop),
        }
    }

    /// One order and the checks on it.
    async fn leg(&self, trip: usize, side: OrderSide, want: Want) -> Result<Leg> {
        let rules = self.rules()?.clone();
        let style = match side {
            OrderSide::Buy => self.settings.buy_order,
            OrderSide::Sell => self.settings.sell_order,
        };
        let live = self.live_for(style)?;
        let rates = self
            .commission
            .clone()
            .ok_or_else(|| anyhow!("the commission was not read"))?;
        let name = stem(trip, side);
        let symbol = self.settings.symbol.clone();
        let rest = live.rest();

        // The book, then the balances: what the order is sized from and what
        // E5 compares.
        let book = self
            .run
            .call(
                CallSpec::new(CASE, Expected::ok()).record_as(&format!("{name}-book")),
                || async { rest.order_book(&symbol, 5).await },
            )
            .await?;
        let read_book = self
            .run
            .gate()
            .exchanges()
            .into_iter()
            .rev()
            .find(|exchange| exchange.path == DEPTH_PATH);
        let (Some(snapshot), Some(read)) = (book.result.ok(), read_book) else {
            return self.not_sized(&book.verdict, "the book was not read");
        };
        let touch = Touch {
            snapshot,
            read_sent_ns: read.sent_ns,
            read_returned_ns: read.returned_ns,
        };
        let before = self
            .run
            .call(
                CallSpec::new(CASE, Expected::ok()).record_as(&format!("{name}-account-before")),
                || async { rest.balances().await },
            )
            .await?;
        let Ok(before) = before.result else {
            return self.not_sized(&before.verdict, "the account's balances were not read");
        };

        // Size it.
        let touched = match side {
            OrderSide::Buy => touch.snapshot.asks.first(),
            OrderSide::Sell => touch.snapshot.bids.first(),
        };
        let Some((price, _)) = touched.copied() else {
            self.run.computed(
                CASE,
                Verdict::Skipped,
                &format!(
                    "the book has nothing on the side a {} takes",
                    side_word(side)
                ),
                None,
            )?;
            return Ok(Leg::Skipped);
        };
        let requested = match want {
            Want::Usd { usd, off_step } => {
                let whole = quantity_for(usd, price, rules.lot_step)?;
                if off_step {
                    whole + rules.lot_step / Decimal::TWO
                } else {
                    whole
                }
            }
            Want::Quantity(quantity) => quantity,
        };
        let rounded = round_down(requested, rules.lot_step)?;
        if let Some(why) = self.cannot_trade(side, style, &rules, &rates, rounded, &touch) {
            self.run.computed(CASE, Verdict::Skipped, &why, None)?;
            return Ok(if side == OrderSide::Buy {
                Leg::Skipped
            } else {
                // A sell that cannot be made leaves the account holding what the
                // buy delivered: the run stops.
                self.run.computed(CASE, Verdict::Fail, &why, None)?;
                Leg::Stopped(None)
            });
        }

        // The ledger's say-so, then the order.
        let permit = match self.run.authorise_order(rounded, price) {
            Ok(permit) => permit,
            Err(stop) => return self.ledger_stop(stop, None),
        };
        let request = OrderRequest {
            symbol: symbol.clone(),
            side,
            quantity: requested,
            quoted_price: price,
            reduce_only: false,
        };
        let gate = self.run.gate();
        let sent = || -> Option<Exchange> {
            gate.exchanges()
                .into_iter()
                .rev()
                .find(|e| e.method == "POST" && e.path == ORDER_PATH)
        };
        let called = self
            .run
            .call_graded(
                CallSpec::new(CASE, Expected::ok()).record_as(&format!("{name}-order")),
                || live.execute(&request),
                |result| {
                    let mut references = order_references(
                        side,
                        Some(&touch),
                        sent().as_ref(),
                        result.as_ref().ok(),
                        Some(&rates),
                    );
                    // How the order was expressed, beside what it was priced against.
                    references["order_style"] = json!(style.word());
                    references["limit_price"] = match style {
                        LegStyle::LimitIoc => json!(price.to_string()),
                        LegStyle::Market => Value::Null,
                    };
                    Some(references)
                },
                |_| Grade::Pass,
            )
            .await?;
        drop(permit);
        let reply = sent().and_then(|exchange| exchange.body);
        let Ok(fill) = called.result else {
            // The line says what happened to the order. If its outcome is unknown
            // it names the order, which the venue can be asked about.
            return Ok(Leg::Stopped(None));
        };

        // The money first, then the checks.
        let booked = self.run.ledger().record_fill(&Fill {
            base_asset: &rules.base_asset,
            quote_asset: &rules.quote_asset,
            buy: side == OrderSide::Buy,
            qty: fill.filled_qty,
            price: fill.filled_price,
            commission: fill.commission,
            commission_asset: &fill.commission_asset,
        });
        if let Err(stop) = booked {
            return self.ledger_stop(stop, Some(fill));
        }

        // E6: from the reply the gate saw.
        let e6 = e6_step_and_time(&fill, rules.lot_step, reply.as_deref());
        if !self.check_line("E6", e6)? {
            return Ok(Leg::Stopped(Some(fill)));
        }

        let Some(client_order_id) = fill.client_order_id.clone() else {
            self.check_line("E1", Err("the fill names no client order id".to_string()))?;
            return Ok(Leg::Stopped(Some(fill)));
        };
        let Some(order_id) = fill.order_ref else {
            self.check_line("E1", Err("the fill names no venue order id".to_string()))?;
            return Ok(Leg::Stopped(Some(fill)));
        };

        // E1: what the venue says became of the order.
        let e1 = self
            .run
            .call_graded(
                CallSpec::new("E1", Expected::ok()).record_as(&format!("{name}-order-status")),
                || live.order_state(&symbol, &client_order_id),
                |_| None,
                |result| match result {
                    Ok(state) => e1_order_state(&fill, rounded, state).into(),
                    Err(_) => Grade::Pass,
                },
            )
            .await?;
        if self.stops(&e1.verdict) {
            return Ok(Leg::Stopped(Some(fill)));
        }

        // E2: the order's own trades.
        let client = rest.client();
        let e2 = self
            .run
            .call_graded(
                CallSpec::new("E2", Expected::ok()).record_as(&format!("{name}-my-trades")),
                || async {
                    trade_lines(client, MY_TRADES_PATH, &symbol, order_id, fill.filled_qty).await
                },
                |_| None,
                |result| match result {
                    Ok(lines) => e2_my_trades(&fill, lines).into(),
                    Err(_) => Grade::Pass,
                },
            )
            .await?;
        if self.stops(&e2.verdict) {
            return Ok(Leg::Stopped(Some(fill)));
        }

        // E3: the same trades, seen from outside the account.
        let e3 = self
            .run
            .call_graded(
                CallSpec::new("E3", Expected::ok()).record_as(&format!("{name}-public-trades")),
                || async { rest.recent_trades(&symbol, 1000).await },
                |_| None,
                |result| match result {
                    Ok(public) => e3_public_trades(&fill, public),
                    Err(_) => Grade::Pass,
                },
            )
            .await?;
        if self.stops(&e3.verdict) {
            return Ok(Leg::Stopped(Some(fill)));
        }

        // E5: the account, to the unit.
        let e5 = self
            .run
            .call_graded(
                CallSpec::new("E5", Expected::ok()).record_as(&format!("{name}-account-after")),
                || async { rest.balances().await },
                |_| None,
                |result| match result {
                    Ok(after) => e5_balances(side, &rules, &fill, &before, after).into(),
                    Err(_) => Grade::Pass,
                },
            )
            .await?;
        if self.stops(&e5.verdict) {
            return Ok(Leg::Stopped(Some(fill)));
        }

        // E4: the commission, from the rate read at the start.
        let proceed = match e4_commission(side, &fill, &rules, &rates) {
            E4::Pass(note) => {
                self.run.computed("E4", Verdict::Pass, &note, None)?;
                true
            }
            E4::SkippedAsset(why) => {
                self.run.computed("E4", Verdict::Skipped, &why, None)?;
                true
            }
            E4::Fail(why) => {
                self.run.computed("E4", Verdict::Fail, &why, None)?;
                false
            }
        };
        Ok(if proceed {
            Leg::Done(fill)
        } else {
            Leg::Stopped(Some(fill))
        })
    }

    /// Whether a leg's order cannot be placed as sized, and why: under the
    /// symbol's minimum, a limit-ioc order the touch is too thin for (it would fill
    /// in part, and a part may be too small to sell back), or (for a buy) too small
    /// for what it delivers to be sold back above the minimum with a one per cent
    /// move against it. `style` is this leg's; the sell leg's decides the sold-back
    /// check.
    fn cannot_trade(
        &self,
        side: OrderSide,
        style: LegStyle,
        rules: &SymbolRules,
        rates: &SymbolCommission,
        quantity: Decimal,
        touch: &Touch,
    ) -> Option<String> {
        if quantity.is_zero() || quantity < rules.min_qty {
            return Some(format!(
                "a quantity of {quantity} is under the symbol's minimum of {}",
                rules.min_qty
            ));
        }
        let (bid, ask) = (
            touch.snapshot.bids.first()?.0,
            touch.snapshot.asks.first()?.0,
        );
        if style == LegStyle::LimitIoc {
            let (price, available) = match side {
                OrderSide::Buy => touch.snapshot.asks.first()?,
                OrderSide::Sell => touch.snapshot.bids.first()?,
            };
            if *available < quantity {
                return Some(format!(
                    "the touch holds {available} {} at {price}, less than the {quantity} a \
                     limit-ioc order at the touch asks for: it would fill in part",
                    rules.base_asset
                ));
            }
        }
        let at_risk = |quantity: Decimal, price: Decimal| quantity.checked_mul(price);
        match side {
            OrderSide::Sell => {
                if let Some(minimum) = minimum_for(style, rules) {
                    if at_risk(quantity, bid).is_none_or(|value| value < minimum) {
                        return Some(format!(
                            "{quantity} {} at {bid} is under the minimum notional of {minimum}: \
                             the buy's quantity cannot be sold back",
                            rules.base_asset
                        ));
                    }
                }
            }
            OrderSide::Buy => {
                if let Some(minimum) = minimum_for(style, rules) {
                    if at_risk(quantity, ask).is_none_or(|value| value < minimum) {
                        return Some(format!(
                            "{quantity} {} at {ask} is under the minimum notional of {minimum}",
                            rules.base_asset
                        ));
                    }
                }
                if let Some(minimum) = minimum_for(self.settings.sell_order, rules) {
                    let rate = effective_rate(OrderSide::Buy, rates).ok()?;
                    let delivered =
                        round_down(quantity.checked_mul(Decimal::ONE - rate)?, rules.lot_step)
                            .ok()?;
                    let back = delivered
                        .checked_mul(bid)?
                        .checked_mul(Decimal::new(99, 2))?;
                    if back < minimum {
                        return Some(format!(
                            "what {quantity} {} delivers would be worth about {back} when sold \
                             back, under the minimum notional of {minimum}",
                            rules.base_asset
                        ));
                    }
                }
            }
        }
        None
    }

    /// A leg that could not be sized, because a read before it did not answer.
    fn not_sized(&self, verdict: &Verdict, why: &str) -> Result<Leg> {
        if matches!(verdict, Verdict::Skipped) {
            self.run.computed(CASE, Verdict::Skipped, why, None)?;
        }
        Ok(Leg::Stopped(None))
    }

    fn ledger_stop(&self, stop: Stop, held: Option<CexFill>) -> Result<Leg> {
        self.run
            .note(CASE, Expected::ok(), Verdict::Halted, &stop.to_string())?;
        Ok(Leg::Stopped(held))
    }

    /// Whether a call's verdict ends the run: a failure, the halt file or a
    /// reply that no row knows.
    fn stops(&self, verdict: &Verdict) -> bool {
        matches!(verdict, Verdict::Fail | Verdict::Halted | Verdict::Unmapped)
    }

    /// Writes the line of a check that reads what it already has, and says
    /// whether the run goes on.
    fn check_line(&self, case: &str, checked: std::result::Result<(), String>) -> Result<bool> {
        match checked {
            Ok(()) => {
                self.run.computed(case, Verdict::Pass, "", None)?;
                Ok(true)
            }
            Err(why) => {
                self.run.computed(case, Verdict::Fail, &why, None)?;
                Ok(false)
            }
        }
    }

    // --- the exit ---

    /// The account's balances once the round trips are done, beside what it held
    /// at the start, and what the ledger counts the run to have lost. Recorded,
    /// not asserted: a round trip costs about twice the commission and the spread.
    async fn exit(&self) -> Result<()> {
        let live = self.live()?;
        let rest = live.rest();
        let end = self
            .run
            .call(
                CallSpec::new("EXIT", Expected::ok()).record_as("99-account-after"),
                || async { rest.balances().await },
            )
            .await?;
        let (Ok(end), Some(start)) = (end.result, self.start.as_ref()) else {
            return Ok(());
        };
        let mut assets: Vec<&str> = start
            .balances
            .iter()
            .chain(&end.balances)
            .map(|balance| balance.asset.as_str())
            .collect();
        assets.sort_unstable();
        assets.dedup();
        let held = |account: &SpotAccountBalances, asset: &str| {
            account
                .balance(asset)
                .map_or(Decimal::ZERO, |b| b.free + b.locked)
        };
        let changes: Vec<String> = assets
            .into_iter()
            .map(|asset| format!("{asset} {:+}", held(&end, asset) - held(start, asset)))
            .collect();
        let loss = self
            .run
            .ledger()
            .loss()
            .map_or_else(|stop| stop.to_string(), |loss| format!("{loss} USD"));
        self.run.computed(
            "EXIT",
            Verdict::Pass,
            &format!(
                "{} round trips; the account changed by {}; the ledger counts a loss of {loss}",
                self.settings.round_trips,
                changes.join(", ")
            ),
            None,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::binance::clock::local_now_ms;
    use crate::cex::binance::sign;
    use crate::production::exchange_checks::ORDER_REFERENCE_FIELDS;
    use crate::production::sizing::is_on_step;
    use crate::production::testing::{fast, Setup};
    use serde_json::{json, Value};
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn d(text: &str) -> Decimal {
        Decimal::from_str(text).unwrap()
    }

    // The key `Setup` gives the trading account.
    const KEY: &str = "AKEY-1234-ABCD";
    const SECRET: &str = "SECRET-9876-WXYZ";

    /// The book of the mock: two levels a side, and a first level too thin for a
    /// 6 USD order, so that every order walks the book. The rates are 0.1 %.
    const ASKS: [(&str, &str); 2] = [("1.0010", "5"), ("1.0020", "500")];
    const BIDS: [(&str, &str); 2] = [("1.0000", "5"), ("0.9990", "500")];

    /// What makes the mock exchange disagree with itself, or with the case.
    #[derive(Clone, Default)]
    struct Knobs {
        /// Every order is refused for want of balance.
        refuses_orders: bool,
        /// An order fills this much more than it asked for, off the lot step.
        extra_fill: Option<Decimal>,
        /// The commission is this many times the rate.
        commission_times: Option<Decimal>,
        /// A buy pays its commission in the quote asset.
        commission_in_quote: bool,
        /// Commission is paid in BNB, which the ledger cannot value.
        commission_in_bnb: bool,
        /// The n-th read (1-based) of an order's `myTrades` lines has the first
        /// line's commission one unit too high.
        my_trades_differ_on_read: Option<u32>,
        /// The public list shows the run's trades a tick above their price.
        public_price_off: bool,
        /// The public list no longer holds the run's trades.
        public_window_empty: bool,
        /// The account lists this much less of the quote asset once an order has
        /// been placed.
        balance_leak: Option<Decimal>,
        /// The first order's reply comes after the client has given up on it.
        slow_first_reply: bool,
        /// The placing reply has no `transactTime`.
        no_transact_time: bool,
        /// The first level of each side holds 500, so that a limit-ioc order at the
        /// touch fills whole.
        deep_touch: bool,
        /// The book an order meets is 0.0010 worse than the one the depth read
        /// showed: the touch moved between the read and the order.
        book_moved: bool,
        /// `exchangeInfo` gives the symbol no `PRICE_FILTER`.
        no_price_filter: bool,
    }

    struct Trade {
        id: u64,
        price: Decimal,
        qty: Decimal,
        commission: Decimal,
        asset: String,
        buyer: bool,
        time_ms: i64,
    }

    struct Order {
        id: u64,
        client_id: String,
        side: String,
        qty: Decimal,
        quote: Decimal,
        time_ms: i64,
        trades: Vec<Trade>,
        /// The limit price of a limit-ioc order; `None` for a market order.
        limit: Option<Decimal>,
        status: &'static str,
    }

    struct State {
        usdt: Decimal,
        aero: Decimal,
        bnb: Decimal,
        next_trade: u64,
        next_order: u64,
        orders: Vec<Order>,
        public: Vec<Trade>,
        my_trades_reads: HashMap<u64, u32>,
    }

    /// A stand-in for Binance spot that keeps an account and a public tape, and
    /// matches market orders against a two-level book. It checks what Binance
    /// checks of these requests: the key, the signature, the timestamp, the lot
    /// step, the minimum notional and the balance. Its replies are shaped as the
    /// documented ones are; the catalogue says which are synthetic.
    #[derive(Clone)]
    struct MiniExchange {
        knobs: Knobs,
        state: Arc<Mutex<State>>,
        /// Called when an order has been matched.
        on_order: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    #[allow(clippy::result_large_err)]
    impl MiniExchange {
        fn new(knobs: Knobs) -> Self {
            let fillers = (990..1000)
                .map(|id| Trade {
                    id,
                    price: d("1.0005"),
                    qty: d("3"),
                    commission: Decimal::ZERO,
                    asset: String::new(),
                    buyer: false,
                    time_ms: 1_700_000_000_000,
                })
                .collect();
            Self {
                state: Arc::new(Mutex::new(State {
                    usdt: d("100"),
                    aero: d("20"),
                    bnb: if knobs.commission_in_bnb {
                        d("5")
                    } else {
                        Decimal::ZERO
                    },
                    next_trade: 1000,
                    next_order: 700,
                    orders: Vec::new(),
                    public: fillers,
                    my_trades_reads: HashMap::new(),
                })),
                knobs,
                on_order: None,
            }
        }

        fn holdings(&self) -> (Decimal, Decimal) {
            let state = self.state.lock().unwrap();
            (state.usdt, state.aero)
        }

        fn orders_placed(&self) -> usize {
            self.state.lock().unwrap().orders.len()
        }

        /// The book the depth read shows, a side of it.
        fn book(&self, buy: bool) -> [(&'static str, &'static str); 2] {
            let (mut asks, mut bids) = (ASKS, BIDS);
            if self.knobs.deep_touch {
                asks[0].1 = "500";
                bids[0].1 = "500";
            }
            if buy {
                asks
            } else {
                bids
            }
        }

        fn refuse(status: u16, code: i64, msg: &str) -> ResponseTemplate {
            ResponseTemplate::new(status).set_body_json(json!({"code": code, "msg": msg}))
        }

        fn ok(body: Value) -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(body)
        }

        fn param(request: &Request, name: &str) -> Option<String> {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        }

        fn authenticate(request: &Request) -> Result<(), ResponseTemplate> {
            let key = request
                .headers
                .get("X-MBX-APIKEY")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if key != KEY {
                return Err(Self::refuse(401, -2014, "API-key format invalid."));
            }
            let query = request.url.query().unwrap_or_default();
            let Some((unsigned, signature)) = query.rsplit_once("&signature=") else {
                return Err(Self::refuse(
                    400,
                    -1102,
                    "Mandatory parameter 'signature' was not sent, was empty/null, or malformed.",
                ));
            };
            if sign::sign(SECRET, unsigned) != signature {
                return Err(Self::refuse(
                    400,
                    -1022,
                    "Signature for this request is not valid.",
                ));
            }
            let timestamp = Self::param(request, "timestamp")
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0);
            let window = Self::param(request, "recvWindow")
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(5_000);
            if (local_now_ms() - timestamp).abs() > window {
                return Err(Self::refuse(
                    400,
                    -1021,
                    "Timestamp for this request is outside of the recvWindow.",
                ));
            }
            Ok(())
        }

        fn levels(levels: &[(&str, &str)]) -> Value {
            Value::Array(
                levels
                    .iter()
                    .map(|(price, qty)| json!([price, qty]))
                    .collect(),
            )
        }

        fn exchange_info(&self) -> Value {
            let mut filters = vec![
                json!({"filterType": "LOT_SIZE", "minQty": "0.10000000",
                       "maxQty": "100000.00000000", "stepSize": "0.10000000"}),
                json!({"filterType": "NOTIONAL", "minNotional": "5.00000000",
                       "applyMinToMarket": true, "maxNotional": "9000000.00000000",
                       "applyMaxToMarket": false, "avgPriceMins": 5}),
            ];
            if !self.knobs.no_price_filter {
                filters.insert(
                    0,
                    json!({"filterType": "PRICE_FILTER", "minPrice": "0.00010000",
                           "maxPrice": "1000.00000000", "tickSize": "0.00010000"}),
                );
            }
            json!({
                "timezone": "UTC", "serverTime": local_now_ms(), "rateLimits": [],
                "exchangeFilters": [],
                "symbols": [{
                    "symbol": "AEROUSDT", "status": "TRADING",
                    "baseAsset": "AERO", "quoteAsset": "USDT",
                    "filters": filters
                }]
            })
        }

        fn account(&self) -> Value {
            let state = self.state.lock().unwrap();
            let leak = if state.orders.is_empty() {
                Decimal::ZERO
            } else {
                self.knobs.balance_leak.unwrap_or_default()
            };
            let mut balances = Vec::new();
            for (asset, free) in [
                ("USDT", state.usdt - leak),
                ("AERO", state.aero),
                ("BNB", state.bnb),
            ] {
                if !free.is_zero() {
                    balances.push(
                        json!({"asset": asset, "free": free.to_string(), "locked": "0.00000000"}),
                    );
                }
            }
            json!({
                "updateTime": 1_700_000_000_000u64, "accountType": "SPOT",
                "balances": balances, "permissions": ["SPOT"], "uid": 354937868
            })
        }

        fn commission() -> Value {
            let block = |taker: &str| json!({"maker": taker, "taker": taker, "buyer": "0.00000000", "seller": "0.00000000"});
            json!({
                "symbol": "AEROUSDT",
                "standardCommission": block("0.00100000"),
                "specialCommission": block("0.00000000"),
                "taxCommission": block("0.00000000"),
                "discount": {"enabledForAccount": true, "enabledForSymbol": true,
                             "discountAsset": "BNB", "discount": "0.75000000"}
            })
        }

        fn trade_rows(&self, order: &Order, reads: u32) -> Value {
            let differ = self.knobs.my_trades_differ_on_read == Some(reads);
            Value::Array(
                order
                    .trades
                    .iter()
                    .enumerate()
                    .map(|(index, trade)| {
                        let commission = if differ && index == 0 {
                            trade.commission + Decimal::new(1, 8)
                        } else {
                            trade.commission
                        };
                        json!({
                            "symbol": "AEROUSDT", "id": trade.id, "orderId": order.id,
                            "orderListId": -1, "price": trade.price.to_string(),
                            "qty": trade.qty.to_string(),
                            "quoteQty": (trade.price * trade.qty).to_string(),
                            "commission": commission.to_string(),
                            "commissionAsset": trade.asset, "time": trade.time_ms,
                            "isBuyer": trade.buyer, "isMaker": false, "isBestMatch": true
                        })
                    })
                    .collect(),
            )
        }

        fn place(&self, request: &Request) -> ResponseTemplate {
            let Some(side) = Self::param(request, "side") else {
                return Self::refuse(400, -1102, "Mandatory parameter 'side' was not sent.");
            };
            let quantity = Self::param(request, "quantity")
                .and_then(|value| Decimal::from_str(&value).ok())
                .unwrap_or_default();
            let client_id = Self::param(request, "newClientOrderId").unwrap_or_default();
            let buy = side == "BUY";
            let book = self.book(buy);
            // A limit order must say its price and be immediate-or-cancel here, and
            // the price must sit on the tick, as Binance's filters require.
            let limit = if Self::param(request, "type").as_deref() == Some("LIMIT") {
                let Some(price) =
                    Self::param(request, "price").and_then(|value| Decimal::from_str(&value).ok())
                else {
                    return Self::refuse(
                        400,
                        -1102,
                        "Mandatory parameter 'price' was not sent, was empty/null, or malformed.",
                    );
                };
                if Self::param(request, "timeInForce").as_deref() != Some("IOC") {
                    return Self::refuse(
                        400,
                        -1102,
                        "Mandatory parameter 'timeInForce' was not sent, was empty/null, or malformed.",
                    );
                }
                if !is_on_step(price, d("0.0001")) {
                    return Self::refuse(400, -1013, "Filter failure: PRICE_FILTER");
                }
                Some(price)
            } else {
                None
            };
            if self.knobs.refuses_orders {
                return Self::refuse(
                    400,
                    -2010,
                    "Account has insufficient balance for requested action.",
                );
            }
            if quantity < d("0.1") || !is_on_step(quantity, d("0.1")) {
                return Self::refuse(400, -1013, "Filter failure: LOT_SIZE");
            }
            if quantity * d(book[0].0) < d("5") {
                return Self::refuse(400, -1013, "Filter failure: NOTIONAL");
            }

            // Walk the book, which may have moved since the depth read. A limit order
            // meets only the levels at its price or better.
            let shift = if self.knobs.book_moved {
                d("0.0010")
            } else {
                Decimal::ZERO
            };
            let mut remaining = quantity;
            let mut matched = Vec::new();
            for (price, available) in book {
                if remaining.is_zero() {
                    break;
                }
                let level = if buy {
                    d(price) + shift
                } else {
                    d(price) - shift
                };
                if let Some(limit) = limit {
                    if (buy && level > limit) || (!buy && level < limit) {
                        break;
                    }
                }
                let used = remaining.min(d(available));
                matched.push((level, used));
                remaining -= used;
            }
            if let (Some(extra), Some(last)) = (self.knobs.extra_fill, matched.last_mut()) {
                last.1 += extra;
            }

            let mut state = self.state.lock().unwrap();
            let total: Decimal = matched.iter().map(|(_, qty)| *qty).sum();
            let quote: Decimal = matched.iter().map(|(price, qty)| *price * *qty).sum();
            if (buy && state.usdt < quote) || (!buy && state.aero < total) {
                return Self::refuse(
                    400,
                    -2010,
                    "Account has insufficient balance for requested action.",
                );
            }
            let rate = Decimal::new(1, 3) * self.knobs.commission_times.unwrap_or(Decimal::ONE);
            let asset = if self.knobs.commission_in_bnb {
                "BNB"
            } else if buy && !self.knobs.commission_in_quote {
                "AERO"
            } else {
                "USDT"
            };
            let time_ms = local_now_ms();
            let mut trades = Vec::new();
            for (price, qty) in &matched {
                let amount = if asset == "AERO" { *qty } else { *price * *qty };
                trades.push(Trade {
                    id: state.next_trade,
                    price: *price,
                    qty: *qty,
                    commission: (amount * rate).round_dp(8),
                    asset: asset.to_string(),
                    buyer: buy,
                    time_ms,
                });
                state.next_trade += 1;
            }
            let commission: Decimal = trades.iter().map(|trade| trade.commission).sum();
            if buy {
                state.usdt -= quote;
                state.aero += total;
            } else {
                state.aero -= total;
                state.usdt += quote;
            }
            match asset {
                "AERO" => state.aero -= commission,
                "USDT" => state.usdt -= commission,
                _ => state.bnb -= commission,
            }
            let id = state.next_order;
            state.next_order += 1;
            let fills: Vec<Value> = trades
                .iter()
                .map(|trade| {
                    json!({
                        "price": trade.price.to_string(), "qty": trade.qty.to_string(),
                        "commission": trade.commission.to_string(),
                        "commissionAsset": trade.asset, "tradeId": trade.id
                    })
                })
                .collect();
            for trade in &trades {
                let price = if self.knobs.public_price_off {
                    trade.price + Decimal::new(1, 4)
                } else {
                    trade.price
                };
                state.public.push(Trade {
                    id: trade.id,
                    price,
                    qty: trade.qty,
                    commission: Decimal::ZERO,
                    asset: String::new(),
                    buyer: false,
                    time_ms,
                });
            }
            // An immediate-or-cancel order that took less than it asked for expires.
            let status = if limit.is_some() && total < quantity {
                "EXPIRED"
            } else {
                "FILLED"
            };
            state.orders.push(Order {
                id,
                client_id: client_id.clone(),
                side: side.clone(),
                qty: total,
                quote,
                time_ms,
                trades,
                limit,
                status,
            });
            drop(state);
            if let Some(on_order) = &self.on_order {
                on_order();
            }
            let mut reply = json!({
                "symbol": "AEROUSDT", "orderId": id, "orderListId": -1,
                "clientOrderId": client_id, "transactTime": time_ms,
                "price": limit.map_or_else(|| "0.00000000".to_string(), |price| price.to_string()),
                "origQty": quantity.to_string(),
                "executedQty": total.to_string(), "cummulativeQuoteQty": quote.to_string(),
                "status": status,
                "timeInForce": if limit.is_some() { "IOC" } else { "GTC" },
                "type": if limit.is_some() { "LIMIT" } else { "MARKET" }, "side": side,
                "fills": fills
            });
            if self.knobs.no_transact_time {
                reply.as_object_mut().unwrap().remove("transactTime");
            }
            Self::ok(reply)
        }

        fn status(&self, request: &Request) -> ResponseTemplate {
            let client_id = Self::param(request, "origClientOrderId").unwrap_or_default();
            let state = self.state.lock().unwrap();
            let Some(order) = state
                .orders
                .iter()
                .find(|order| order.client_id == client_id)
            else {
                return Self::refuse(400, -2013, "Order does not exist.");
            };
            Self::ok(json!({
                "symbol": "AEROUSDT", "orderId": order.id, "orderListId": -1,
                "clientOrderId": order.client_id,
                "price": order.limit.map_or_else(|| "0.00000000".to_string(), |price| price.to_string()),
                "origQty": order.qty.to_string(), "executedQty": order.qty.to_string(),
                "cummulativeQuoteQty": order.quote.to_string(), "status": order.status,
                "timeInForce": if order.limit.is_some() { "IOC" } else { "GTC" },
                "type": if order.limit.is_some() { "LIMIT" } else { "MARKET" },
                "side": order.side,
                "time": order.time_ms, "updateTime": order.time_ms + 5,
                "isWorking": true, "origQuoteOrderQty": "0.00000000"
            }))
        }

        fn my_trades(&self, request: &Request) -> ResponseTemplate {
            let order_id: u64 = Self::param(request, "orderId")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default();
            let mut state = self.state.lock().unwrap();
            let reads = {
                let reads = state.my_trades_reads.entry(order_id).or_default();
                *reads += 1;
                *reads
            };
            match state.orders.iter().find(|order| order.id == order_id) {
                Some(order) => Self::ok(self.trade_rows(order, reads)),
                None => Self::ok(json!([])),
            }
        }

        fn public_trades(&self) -> ResponseTemplate {
            let state = self.state.lock().unwrap();
            let rows: Vec<Value> = state
                .public
                .iter()
                .filter(|trade| !self.knobs.public_window_empty || trade.id < 1000)
                .map(|trade| {
                    json!({
                        "id": trade.id, "price": trade.price.to_string(),
                        "qty": trade.qty.to_string(),
                        "quoteQty": (trade.price * trade.qty).to_string(),
                        "time": trade.time_ms, "isBuyerMaker": !trade.buyer,
                        "isBestMatch": true
                    })
                })
                .collect();
            Self::ok(Value::Array(rows))
        }
    }

    #[allow(clippy::result_large_err)]
    impl Respond for MiniExchange {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            match (request.method.as_str(), request.url.path()) {
                ("GET", "/api/v3/time") => Self::ok(json!({"serverTime": local_now_ms()})),
                ("GET", "/api/v3/exchangeInfo") => Self::ok(self.exchange_info()),
                ("GET", "/api/v3/depth") => Self::ok(json!({
                    "lastUpdateId": 1_027_024,
                    "bids": Self::levels(&self.book(false)), "asks": Self::levels(&self.book(true))
                })),
                ("GET", "/api/v3/trades") => self.public_trades(),
                (verb, path) => {
                    if let Err(refusal) = Self::authenticate(request) {
                        return refusal;
                    }
                    match (verb, path) {
                        ("GET", "/api/v3/account") => Self::ok(self.account()),
                        ("GET", "/api/v3/account/commission") => Self::ok(Self::commission()),
                        ("POST", "/api/v3/order") => {
                            let slow = self.knobs.slow_first_reply && self.orders_placed() == 0;
                            let reply = self.place(request);
                            if slow {
                                reply.set_delay(Duration::from_millis(900))
                            } else {
                                reply
                            }
                        }
                        ("GET", "/api/v3/order") => self.status(request),
                        ("GET", "/api/v3/myTrades") => self.my_trades(request),
                        _ => ResponseTemplate::new(404),
                    }
                }
            }
        }
    }

    fn settings(round_trips: usize) -> Settings {
        Settings {
            symbol: "AEROUSDT".to_string(),
            order_usd: d("10"),
            round_trips,
            buy_order: LegStyle::Market,
            sell_order: LegStyle::Market,
        }
    }

    /// `settings` with the order style of each leg.
    fn styled(round_trips: usize, buy: LegStyle, sell: LegStyle) -> Settings {
        Settings {
            buy_order: buy,
            sell_order: sell,
            ..settings(round_trips)
        }
    }

    /// A run against a mock exchange, in a scratch directory.
    struct Lab {
        server: MockServer,
        setup: Setup,
        exchange: MiniExchange,
    }

    impl Lab {
        async fn new(name: &str, knobs: Knobs) -> Self {
            Self::with_exchange(name, MiniExchange::new(knobs)).await
        }

        async fn with_exchange(name: &str, exchange: MiniExchange) -> Self {
            let server = MockServer::start().await;
            Mock::given(any())
                .respond_with(exchange.clone())
                .mount(&server)
                .await;
            let setup = Setup::new(name, &server);
            Self {
                server,
                setup,
                exchange,
            }
        }

        fn with(mut self, key: &str, value: &str) -> Self {
            self.setup = self.setup.with(key, value);
            self
        }

        async fn received(&self) -> Vec<String> {
            self.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|request| format!("{} {}", request.method, request.url.path()))
                .collect()
        }

        /// The parameters of every order the mock received, in the order sent.
        async fn orders_sent(&self) -> Vec<HashMap<String, String>> {
            self.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| {
                    request.method.as_str() == "POST" && request.url.path() == ORDER_PATH
                })
                .map(|request| {
                    request
                        .url
                        .query_pairs()
                        .map(|(key, value)| (key.into_owned(), value.into_owned()))
                        .collect()
                })
                .collect()
        }
    }

    /// What a run left: its lines, and whether it was clean.
    struct Played {
        lines: Vec<Value>,
        clean: bool,
        loss: Decimal,
    }

    impl Played {
        fn cases(&self) -> Vec<(&str, &str)> {
            self.lines
                .iter()
                .map(|line| {
                    (
                        line["case"].as_str().unwrap(),
                        line["verdict"].as_str().unwrap(),
                    )
                })
                .collect()
        }

        fn lines_of(&self, case: &str) -> Vec<&Value> {
            self.lines
                .iter()
                .filter(|line| line["case"] == case)
                .collect()
        }

        fn orders(&self) -> Vec<&Value> {
            self.lines
                .iter()
                .filter(|line| line["request"]["path"] == "/api/v3/order" && line["case"] == CASE)
                .collect()
        }

        fn last(&self) -> &Value {
            self.lines.last().unwrap()
        }
    }

    async fn play(lab: &Lab, settings: Settings) -> Played {
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        play_with(lab, &run, &env, settings).await
    }

    async fn play_with(lab: &Lab, run: &Run, env: &dyn Env, settings: Settings) -> Played {
        let mut lv2a = Lv2aExchange::new(run, env, lab.server.uri(), fast(), settings);
        lv2a.run_all().await.unwrap();
        Played {
            lines: lab.setup.lines(),
            clean: run.finish().is_ok(),
            loss: run.ledger().loss().unwrap_or_default(),
        }
    }

    /// The nine lines of a leg that passes.
    fn leg_cases() -> Vec<(&'static str, &'static str)> {
        vec![
            ("LV2a", "pass"), // the book
            ("LV2a", "pass"), // the account before
            ("LV2a", "pass"), // the order
            ("E6", "pass"),
            ("E1", "pass"),
            ("E2", "pass"),
            ("E3", "pass"),
            ("E5", "pass"),
            ("E4", "pass"),
        ]
    }

    // --- one round trip, every check passing ---

    #[tokio::test]
    async fn a_round_trip_passes_every_check_and_writes_the_lines_of_the_spec() {
        let lab = Lab::new("trip", Knobs::default()).await;
        let played = play(&lab, settings(1)).await;

        let mut expected = vec![("LV2a", "pass"); 3]; // rules, commission, account
        expected.extend(leg_cases()); // the buy
        expected.extend(leg_cases()); // the sell
        expected.extend([("EXIT", "pass"), ("EXIT", "pass")]);
        assert_eq!(played.cases(), expected);
        assert!(played.clean);
        assert_eq!(lab.exchange.orders_placed(), 2);

        // The first round trip is about 6 USD: 6 / 1.0010 is 5.99 AERO, 5.9 to the step.
        let orders = played.orders();
        assert_eq!(orders[0]["request"]["params"]["side"], "BUY");
        assert_eq!(orders[0]["request"]["params"]["quantity"], "5.9");
        // The sell is the buy's delivery, 5.9 less 0.0059 of commission, to the step.
        assert_eq!(orders[1]["request"]["params"]["side"], "SELL");
        assert_eq!(orders[1]["request"]["params"]["quantity"], "5.8");
        lab.setup.clean_up();
    }

    /// Every order's `references` is an object with each field of the spec's
    /// table, and what was measured is where it should be.
    #[tokio::test]
    async fn an_order_carries_the_references_of_the_spec_and_a_read_carries_none() {
        let lab = Lab::new("refs", Knobs::default()).await;
        let played = play(&lab, settings(1)).await;

        for order in played.orders() {
            let references = &order["references"];
            for field in ORDER_REFERENCE_FIELDS {
                assert!(references.get(field).is_some(), "{field}: {references}");
            }
            assert!(references["booked_touch"].is_null());
            let (sent, returned) = (
                references["sent_ns"].as_u64().unwrap(),
                references["returned_ns"].as_u64().unwrap(),
            );
            assert!(sent > 0 && sent <= returned, "{sent} {returned}");
            assert!(
                references["touch_at_send"]["read_returned_ns"]
                    .as_u64()
                    .unwrap()
                    <= sent,
                "the book was read before the order left"
            );
            assert_eq!(references["touch_at_send"]["last_update_id"], 1_027_024);
            assert!(references["transact_time_ms"].as_u64().unwrap() > 1_700_000_000_000);
            assert_eq!(references["rate"]["taker"], "0.00100000");
            assert!(references["avg_fill"]["lines"].as_array().unwrap().len() >= 2);
        }
        // The buy of 5.9 walks the asks: 5 at the first level, 0.9 at the second.
        let depth = &played.orders()[0]["references"]["depth_walked"];
        assert_eq!(depth["side"], "asks");
        assert_eq!(depth["levels"][0]["used_qty"], "5");
        assert_eq!(depth["levels"][1]["used_qty"], "0.9");
        assert_eq!(
            played.orders()[0]["references"]["touch_at_send"]["best_ask"]["price"],
            "1.0010"
        );
        // Everything that is not an order says null.
        for line in &played.lines {
            if !(line["case"] == CASE && line["request"]["path"] == "/api/v3/order") {
                assert!(line["references"].is_null(), "{line}");
            }
        }
        lab.setup.clean_up();
    }

    /// Ten round trips: twenty orders, the second round trip's quantity needing
    /// rounding, every figure reconciled and the ledger's loss small.
    #[tokio::test]
    async fn ten_round_trips_reconcile_to_the_unit_and_the_second_needs_rounding() {
        let lab = Lab::new("ten", Knobs::default()).await;
        let played = play(
            &lab,
            Settings {
                round_trips: ROUND_TRIPS,
                ..settings(1)
            },
        )
        .await;

        assert!(played.clean, "{:#?}", played.cases());
        assert_eq!(lab.exchange.orders_placed(), 20);
        let orders = played.orders();
        assert_eq!(orders.len(), 20);
        for order in &orders {
            let quantity = d(order["request"]["params"]["quantity"].as_str().unwrap());
            assert!(is_on_step(quantity, d("0.1")), "{quantity}");
        }
        // 6 USD, then 10 USD asked for as 9.95 and rounded, then 10 USD.
        let buys: Vec<&str> = orders
            .iter()
            .step_by(2)
            .map(|order| order["request"]["params"]["quantity"].as_str().unwrap())
            .collect();
        assert_eq!(&buys[..3], ["5.9", "9.9", "9.9"]);
        assert!(played.lines.iter().all(|line| line["verdict"] != "fail"));
        // Twice the commission and the spread, ten times, on about 10 USD.
        assert!(
            played.loss > Decimal::ZERO && played.loss < d("1"),
            "{}",
            played.loss
        );
        let (usdt, aero) = lab.exchange.holdings();
        // The spread, the commissions and the dust each trip leaves unsold.
        assert!(usdt < d("100") && usdt > d("98"), "{usdt}");
        assert!(
            aero > d("20") && aero < d("21"),
            "dust only, under a step a trip: {aero}"
        );
        let exit = played.last();
        assert_eq!(exit["case"], "EXIT");
        assert!(exit["reason"].as_str().unwrap().contains("10 round trips"));
        lab.setup.clean_up();
    }

    /// A pilot asks for fewer round trips than the ten of a full run.
    #[test]
    fn a_pilot_asks_for_fewer_round_trips_and_a_bad_count_is_refused_by_name() {
        let env = crate::production::env::MapEnv::new(&[("VP_ROUND_TRIPS", "2")]);
        assert_eq!(Settings::from_env(&env).unwrap().round_trips, 2);
        for bad in ["0", "-1", "2.5", "ten"] {
            let env = crate::production::env::MapEnv::new(&[("VP_ROUND_TRIPS", bad)]);
            let why = format!("{:#}", Settings::from_env(&env).unwrap_err());
            assert!(
                why.contains("VP_ROUND_TRIPS") && why.contains(bad),
                "{bad}: {why}"
            );
        }
    }

    #[test]
    fn the_first_round_trip_is_about_six_dollars_and_the_second_is_asked_for_off_the_step() {
        let env = crate::production::env::MapEnv::new(&[]);
        let settings = Settings::from_env(&env).unwrap();
        assert_eq!(
            (
                settings.symbol.as_str(),
                settings.order_usd,
                settings.round_trips
            ),
            ("AEROUSDT", d("10"), 10)
        );

        let server_less = |order_usd: &str| {
            let mut s = settings.clone();
            s.order_usd = d(order_usd);
            s
        };
        let rules = |minimum: Option<&str>| SymbolRules {
            symbol: "AEROUSDT".to_string(),
            status: "TRADING".to_string(),
            base_asset: "AERO".to_string(),
            quote_asset: "USDT".to_string(),
            lot_step: d("0.1"),
            min_qty: d("0.1"),
            min_notional: minimum.map(d),
            apply_min_to_market: Some(true),
        };
        let usd = |want: Want| match want {
            Want::Usd { usd, off_step } => (usd, off_step),
            Want::Quantity(_) => panic!("a quantity"),
        };
        let run_for = |order_usd: &str| {
            // `want` reads nothing of the run: any settings will do.
            (server_less(order_usd), rules(Some("5")))
        };
        // The want does not touch the run, so build the case's pieces by hand.
        let (settings, rules5) = run_for("10");
        let want = |settings: &Settings, rules: &SymbolRules, trip: usize| {
            let case_settings = settings.clone();
            // Mirrors `Lv2aExchange::want`, which needs a run only to hold a reference.
            let lab = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            lab.block_on(async move {
                let server = MockServer::start().await;
                let setup = Setup::new("want", &server);
                let run = setup.run(&server).unwrap();
                let env = setup.env();
                let case = Lv2aExchange::new(&run, &env, server.uri(), fast(), case_settings);
                let want = case.want(trip, rules);
                setup.clean_up();
                want
            })
        };
        assert_eq!(usd(want(&settings, &rules5, 0)), (d("6"), false));
        assert_eq!(usd(want(&settings, &rules5, 1)), (d("10"), true));
        assert_eq!(usd(want(&settings, &rules5, 2)), (d("10"), false));
        // Under 6 USD of order size: the order size.
        assert_eq!(usd(want(&server_less("4"), &rules5, 0)), (d("4"), false));
        // A minimum of 10 wants 12 for the first, which is over the order size.
        assert_eq!(
            usd(want(&settings, &rules(Some("10")), 0)),
            (d("10"), false)
        );
        // A minimum of 6 wants 8 (7.2 rounded up).
        assert_eq!(usd(want(&settings, &rules(Some("6")), 0)), (d("8"), false));
    }

    // --- each check fails on the answer that breaks it ---

    /// The lines up to and including the first failing one, and nothing after it.
    fn assert_stops_at(played: &Played, case: &str, reason: &str) {
        let failing = played
            .lines
            .iter()
            .position(|line| line["verdict"] == "fail")
            .unwrap_or_else(|| panic!("nothing failed: {:#?}", played.cases()));
        let line = &played.lines[failing];
        assert_eq!(line["case"], case, "{:#?}", played.cases());
        assert!(
            line["reason"].as_str().unwrap().contains(reason),
            "{}",
            line["reason"]
        );
        assert!(!played.clean);
        let after: Vec<_> = played.lines[failing + 1..]
            .iter()
            .filter(|line| line["case"] != "EXIT")
            .collect();
        assert!(after.is_empty(), "the run goes on after {case}: {after:#?}");
    }

    #[tokio::test]
    async fn e6_a_fill_off_the_lot_step_stops_the_run() {
        let knobs = Knobs {
            extra_fill: Some(d("0.05")),
            ..Knobs::default()
        };
        let lab = Lab::new("e6-step", knobs).await;
        let played = play(&lab, settings(1)).await;
        assert_stops_at(&played, "E6", "not a whole number of steps");
        // The buy filled and the run holds it: the line says so.
        assert_eq!(played.last()["case"], "EXIT");
        assert!(played.last()["reason"].as_str().unwrap().contains("holds"));
        assert_eq!(
            lab.exchange.orders_placed(),
            1,
            "no sell after a failed check"
        );
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn e6_a_reply_with_no_transact_time_stops_the_run() {
        let knobs = Knobs {
            no_transact_time: true,
            ..Knobs::default()
        };
        let lab = Lab::new("e6-time", knobs).await;
        let played = play(&lab, settings(1)).await;
        assert_stops_at(&played, "E6", "transactTime");
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn e1_a_state_that_is_not_the_fill_stops_the_run() {
        let knobs = Knobs {
            my_trades_differ_on_read: Some(1),
            ..Knobs::default()
        };
        let lab = Lab::new("e1", knobs).await;
        let played = play(&lab, settings(1)).await;
        assert_stops_at(&played, "E1", "not the fill execute returned");
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn e2_trade_lines_that_are_not_the_fills_trades_stop_the_run() {
        let knobs = Knobs {
            my_trades_differ_on_read: Some(2),
            ..Knobs::default()
        };
        let lab = Lab::new("e2", knobs).await;
        let played = play(&lab, settings(1)).await;
        assert_stops_at(&played, "E2", "myTrades lines are not the fill's trades");
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn e3_a_public_trade_with_another_price_stops_the_run() {
        let knobs = Knobs {
            public_price_off: true,
            ..Knobs::default()
        };
        let lab = Lab::new("e3", knobs).await;
        let played = play(&lab, settings(1)).await;
        assert_stops_at(&played, "E3", "public list");
        lab.setup.clean_up();
    }

    /// A trade the window no longer holds is skipped, never a pass, and the
    /// checks after it still run.
    #[tokio::test]
    async fn e3_a_trade_the_window_no_longer_holds_is_skipped_window_and_the_run_goes_on() {
        let knobs = Knobs {
            public_window_empty: true,
            ..Knobs::default()
        };
        let lab = Lab::new("e3-window", knobs).await;
        let played = play(&lab, settings(1)).await;

        let e3 = played.lines_of("E3");
        assert_eq!(e3.len(), 2);
        for line in e3 {
            assert_eq!(line["verdict"], "skipped");
            assert!(line["reason"]
                .as_str()
                .unwrap()
                .starts_with("skipped_window"));
        }
        assert!(!played.lines_of("E5").is_empty(), "the checks after it ran");
        assert!(played.lines.iter().all(|line| line["verdict"] != "fail"));
        assert!(played.clean, "a skip is not a failure");
        lab.setup.clean_up();
    }

    /// The venue deducts twice the rate, consistently: only E4 sees it.
    #[tokio::test]
    async fn e4_a_commission_the_rate_does_not_explain_stops_the_run() {
        let knobs = Knobs {
            commission_times: Some(d("2")),
            ..Knobs::default()
        };
        let lab = Lab::new("e4", knobs).await;
        let played = play(&lab, settings(1)).await;
        assert_stops_at(&played, "E4", "at the buy rate of 0.001");
        for passing in ["E6", "E1", "E2", "E3", "E5"] {
            assert_eq!(played.lines_of(passing)[0]["verdict"], "pass", "{passing}");
        }
        lab.setup.clean_up();
    }

    /// A buy that pays its commission in the quote asset is not paid in the asset
    /// received: E4 says so and is skipped, and the ledger can still value it.
    #[tokio::test]
    async fn e4_a_commission_in_the_other_asset_is_skipped_asset_never_a_pass() {
        let knobs = Knobs {
            commission_in_quote: true,
            ..Knobs::default()
        };
        let lab = Lab::new("e4-asset", knobs).await;
        let played = play(&lab, settings(1)).await;

        let e4 = played.lines_of("E4");
        assert_eq!(e4[0]["verdict"], "skipped");
        assert!(e4[0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("skipped_asset"));
        assert_eq!(
            e4[1]["verdict"], "pass",
            "the sell pays in the asset it receives"
        );
        assert!(played.clean, "{:#?}", played.cases());
        lab.setup.clean_up();
    }

    /// BNB has no price in USD to the ledger, so the run stops, with the reason.
    #[tokio::test]
    async fn a_commission_the_ledger_cannot_value_stops_the_run() {
        let knobs = Knobs {
            commission_in_bnb: true,
            ..Knobs::default()
        };
        let lab = Lab::new("bnb", knobs).await;
        let played = play(&lab, settings(1)).await;

        let halted = played.lines_of(CASE).into_iter().last().unwrap();
        assert_eq!(halted["verdict"], "halted");
        assert!(
            halted["reason"].as_str().unwrap().contains("BNB"),
            "{halted}"
        );
        assert!(
            played.lines_of("E6").is_empty(),
            "no check after the ledger stopped the run"
        );
        assert!(!played.clean);
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn e5_an_account_that_did_not_change_by_the_fill_stops_the_run() {
        let knobs = Knobs {
            balance_leak: Some(d("0.5")),
            ..Knobs::default()
        };
        let lab = Lab::new("e5", knobs).await;
        let played = play(&lab, settings(1)).await;
        assert_stops_at(&played, "E5", "USDT changed by");
        lab.setup.clean_up();
    }

    // --- the rest of what can go wrong ---

    #[tokio::test]
    async fn an_order_the_venue_refuses_is_a_failure_and_stops_the_run() {
        let knobs = Knobs {
            refuses_orders: true,
            ..Knobs::default()
        };
        let lab = Lab::new("refused", knobs).await;
        let played = play(&lab, settings(1)).await;

        let order = played.orders()[0];
        assert_eq!(order["verdict"], "fail");
        assert_eq!(order["outcome"]["code"], -2010);
        assert!(order["references"]["avg_fill"].is_null());
        assert!(!order["references"]["touch_at_send"].is_null());
        assert_eq!(lab.exchange.orders_placed(), 0);
        assert_eq!(played.lines_of("E6").len(), 0);
        lab.setup.clean_up();
    }

    /// The reply to the placing call comes too late to be read. The adapter finds
    /// the order by its client order id, and E6 says the reply was never seen.
    #[tokio::test]
    async fn a_lost_reply_is_recovered_by_the_status_query_and_e6_says_it_never_saw_the_reply() {
        let knobs = Knobs {
            slow_first_reply: true,
            ..Knobs::default()
        };
        let lab = Lab::new("lost", knobs).await;
        let played = play(&lab, settings(1)).await;

        let order = played.orders()[0];
        assert_eq!(order["verdict"], "pass", "the adapter recovered the fill");
        assert!(order["references"]["sent_ns"].as_u64().is_some());
        assert!(
            order["references"]["returned_ns"].is_null(),
            "a request that got no reply has no return time"
        );
        assert_stops_at(&played, "E6", "was not seen");
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn the_halt_file_stops_the_run_before_the_next_call() {
        let lab_halt = lab_halt_on_first_order("halt").await;
        let played = play(&lab_halt, settings(1)).await;

        let halted = played
            .cases()
            .iter()
            .position(|(_, verdict)| *verdict == "halted");
        assert!(halted.is_some(), "{:#?}", played.cases());
        assert_eq!(played.lines_of("E1")[0]["verdict"], "halted");
        assert_eq!(
            lab_halt.exchange.orders_placed(),
            1,
            "nothing after the halt"
        );
        assert!(!played.clean);
        lab_halt.setup.clean_up();
    }

    async fn lab_halt_on_first_order(name: &str) -> Lab {
        // The halt file's path is the setup's, known only once the lab exists:
        // the exchange writes it through a shared cell.
        let cell: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::new(Mutex::new(None));
        let mut exchange = MiniExchange::new(Knobs::default());
        let hook = Arc::clone(&cell);
        exchange.on_order = Some(Arc::new(move || {
            if let Some(path) = hook.lock().unwrap().as_ref() {
                std::fs::write(path, "stop").unwrap();
            }
        }));
        let lab = Lab::with_exchange(name, exchange).await;
        *cell.lock().unwrap() = Some(lab.setup.halt.clone());
        lab
    }

    #[tokio::test]
    async fn the_ledger_refuses_an_order_over_the_cap_before_anything_is_signed() {
        let lab = Lab::new("order-cap", Knobs::default())
            .await
            .with("VP_ORDER_CAP_USD", "5.5");
        let played = play(&lab, settings(1)).await;

        let refused = played.lines_of(CASE).into_iter().last().unwrap();
        assert_eq!(refused["verdict"], "halted");
        assert!(
            refused["reason"].as_str().unwrap().contains("order cap"),
            "{refused}"
        );
        assert_eq!(lab.exchange.orders_placed(), 0);
        assert!(!lab
            .received()
            .await
            .iter()
            .any(|call| call == "POST /api/v3/order"));
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn the_ledger_refuses_an_order_that_could_pass_the_spend_cap() {
        let lab = Lab::new("spend-cap", Knobs::default()).await;
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        run.ledger().record_loss(d("25")).unwrap();
        let played = play_with(&lab, &run, &env, settings(1)).await;

        let refused = played.lines_of(CASE).into_iter().last().unwrap();
        assert_eq!(refused["verdict"], "halted");
        assert!(
            refused["reason"]
                .as_str()
                .unwrap()
                .contains("could pass the cap"),
            "{refused}"
        );
        assert_eq!(lab.exchange.orders_placed(), 0);
        lab.setup.clean_up();
    }

    /// An order too small for what it delivers to be sold back is not placed at
    /// all: a buy that could not be undone is a position, not a round trip.
    #[tokio::test]
    async fn a_buy_that_could_not_be_sold_back_is_skipped_with_its_reason_and_nothing_is_sent() {
        let lab = Lab::new("unsellable", Knobs::default()).await;
        let small = Settings {
            order_usd: d("5.2"),
            ..settings(2)
        };
        let played = play(&lab, small).await;

        assert_eq!(lab.exchange.orders_placed(), 0);
        let skipped: Vec<_> = played
            .lines
            .iter()
            .filter(|line| line["verdict"] == "skipped")
            .collect();
        assert_eq!(
            skipped.len(),
            2,
            "one for each round trip: {:#?}",
            played.cases()
        );
        assert!(skipped[0]["reason"].as_str().unwrap().contains("sold back"));
        assert!(played.clean);
        lab.setup.clean_up();
    }

    // --- one leg a limit-ioc order, the other a market order ---

    /// The order styles of a round trip, as the first run was asked for: an IOC buy at
    /// the ask seen, then a market sell of what it delivered. Every check passes, the
    /// buy is a LIMIT / IOC at the touch on the wire, and the lines say how each was sent.
    #[tokio::test]
    async fn a_limit_ioc_buy_and_a_market_sell_make_a_round_trip_that_passes_every_check() {
        let lab = Lab::new(
            "ioc-buy",
            Knobs {
                deep_touch: true,
                ..Knobs::default()
            },
        )
        .await;
        let played = play(&lab, styled(1, LegStyle::LimitIoc, LegStyle::Market)).await;

        let mut expected = vec![("LV2a", "pass"); 3];
        expected.extend(leg_cases());
        expected.extend(leg_cases());
        expected.extend([("EXIT", "pass"), ("EXIT", "pass")]);
        assert_eq!(played.cases(), expected, "{:#?}", played.lines);
        assert!(played.clean);

        let sent = lab.orders_sent().await;
        assert_eq!(sent.len(), 2);
        assert_eq!(
            (sent[0]["side"].as_str(), sent[0]["type"].as_str()),
            ("BUY", "LIMIT")
        );
        assert_eq!(sent[0]["timeInForce"], "IOC");
        assert_eq!(d(&sent[0]["price"]), d("1.0010"), "the ask the book showed");
        assert_eq!(sent[0]["quantity"], "5.9");
        assert_eq!(
            (sent[1]["side"].as_str(), sent[1]["type"].as_str()),
            ("SELL", "MARKET")
        );
        assert!(!sent[1].contains_key("price") && !sent[1].contains_key("timeInForce"));

        let orders = played.orders();
        let (buy, sell) = (&orders[0]["references"], &orders[1]["references"]);
        assert_eq!(buy["order_style"], "limit-ioc");
        assert_eq!(buy["limit_price"], "1.0010");
        assert_eq!(sell["order_style"], "market");
        assert!(sell["limit_price"].is_null());
        // The IOC buy filled whole at the price seen; the touch held enough for it.
        assert_eq!(d(buy["avg_fill"]["price"].as_str().unwrap()), d("1.0010"));
        assert_eq!(d(buy["avg_fill"]["qty"].as_str().unwrap()), d("5.9"));
        lab.setup.clean_up();
    }

    /// The other way round: a market buy, then an IOC sell at the bid seen.
    #[tokio::test]
    async fn a_market_buy_and_a_limit_ioc_sell_make_a_round_trip_that_passes_every_check() {
        let lab = Lab::new(
            "ioc-sell",
            Knobs {
                deep_touch: true,
                ..Knobs::default()
            },
        )
        .await;
        let played = play(&lab, styled(1, LegStyle::Market, LegStyle::LimitIoc)).await;

        assert!(played.clean, "{:#?}", played.lines);
        let sent = lab.orders_sent().await;
        assert_eq!(sent[0]["type"], "MARKET");
        assert_eq!(
            (sent[1]["type"].as_str(), sent[1]["timeInForce"].as_str()),
            ("LIMIT", "IOC")
        );
        assert_eq!(d(&sent[1]["price"]), d("1.0000"), "the bid the book showed");
        let orders = played.orders();
        assert_eq!(orders[1]["references"]["order_style"], "limit-ioc");
        assert_eq!(orders[1]["references"]["limit_price"], "1.0000");
        lab.setup.clean_up();
    }

    /// If the touch moved away between the read and the order, an IOC at the old
    /// touch fills nothing: the run stops with nothing held and no sell sent. A
    /// market order would have filled at the worse price.
    #[tokio::test]
    async fn a_limit_ioc_buy_the_book_moved_away_from_fills_nothing_and_the_run_stops_flat() {
        let lab = Lab::new(
            "ioc-moved",
            Knobs {
                deep_touch: true,
                book_moved: true,
                ..Knobs::default()
            },
        )
        .await;
        let played = play(&lab, styled(1, LegStyle::LimitIoc, LegStyle::Market)).await;

        assert_eq!(lab.orders_sent().await.len(), 1, "no sell follows");
        assert!(!played.clean, "{:#?}", played.cases());
        assert_eq!(
            lab.exchange.holdings(),
            (d("100"), d("20")),
            "nothing was bought"
        );
        let order = played.orders()[0];
        assert_ne!(order["verdict"], "pass", "{order}");
        assert_eq!(order["references"]["order_style"], "limit-ioc");
        lab.setup.clean_up();
    }

    /// The same moved book with market orders fills, at the worse price: the
    /// difference the IOC refuses to pay.
    #[tokio::test]
    async fn a_market_buy_on_the_moved_book_fills_at_the_worse_price() {
        let lab = Lab::new(
            "market-moved",
            Knobs {
                deep_touch: true,
                book_moved: true,
                ..Knobs::default()
            },
        )
        .await;
        let played = play(&lab, settings(1)).await;

        assert!(played.clean, "{:#?}", played.lines);
        let buy = &played.orders()[0]["references"];
        assert_eq!(d(buy["avg_fill"]["price"].as_str().unwrap()), d("1.0020"));
        assert_eq!(buy["touch_at_send"]["best_ask"]["price"], "1.0010");
        assert_eq!(buy["order_style"], "market");
        lab.setup.clean_up();
    }

    /// An IOC at the touch fills only what the touch holds, so a touch thinner than
    /// the order would fill in part, and a part may be too small to sell back: the
    /// leg is not sent.
    #[tokio::test]
    async fn a_limit_ioc_leg_the_touch_is_too_thin_for_is_skipped_and_nothing_is_sent() {
        let lab = Lab::new("ioc-thin", Knobs::default()).await; // 5 at the touch, 5.9 wanted
        let played = play(&lab, styled(1, LegStyle::LimitIoc, LegStyle::Market)).await;

        assert!(lab.orders_sent().await.is_empty());
        let skipped: Vec<_> = played
            .lines
            .iter()
            .filter(|line| line["verdict"] == "skipped")
            .collect();
        assert_eq!(skipped.len(), 1, "{:#?}", played.cases());
        assert!(
            skipped[0]["reason"]
                .as_str()
                .unwrap()
                .contains("fill in part"),
            "{}",
            skipped[0]["reason"]
        );
        assert!(played.clean);
        lab.setup.clean_up();
    }

    /// A limit order is priced on the symbol's tick; without one in exchangeInfo the
    /// run stops before any order, by name.
    #[tokio::test]
    async fn a_limit_ioc_leg_without_a_price_filter_stops_the_run_before_any_order() {
        let lab = Lab::new(
            "ioc-no-tick",
            Knobs {
                deep_touch: true,
                no_price_filter: true,
                ..Knobs::default()
            },
        )
        .await;
        let played = play(&lab, styled(1, LegStyle::LimitIoc, LegStyle::Market)).await;

        assert!(lab.orders_sent().await.is_empty());
        assert!(!played.clean);
        assert!(
            played.lines.iter().any(|line| line["verdict"] == "fail"
                && line["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("PRICE_FILTER"))),
            "{:#?}",
            played.cases()
        );
        lab.setup.clean_up();
    }

    /// Market orders are what the case sent before the setting, and stay the default.
    #[test]
    fn each_legs_order_style_is_read_from_the_environment_and_defaults_to_market() {
        use crate::production::env::MapEnv;

        let none = Settings::from_env(&MapEnv::new(&[])).unwrap();
        assert_eq!(
            (none.buy_order, none.sell_order),
            (LegStyle::Market, LegStyle::Market)
        );
        let mixed = Settings::from_env(&MapEnv::new(&[
            ("VP_BUY_ORDER", "limit-ioc"),
            ("VP_SELL_ORDER", "market"),
        ]))
        .unwrap();
        assert_eq!(
            (mixed.buy_order, mixed.sell_order),
            (LegStyle::LimitIoc, LegStyle::Market)
        );
        for bad in ["limit", "LIMIT-IOC", "ioc"] {
            let why = Settings::from_env(&MapEnv::new(&[("VP_SELL_ORDER", bad)]))
                .unwrap_err()
                .to_string();
            assert!(
                why.contains("VP_SELL_ORDER") && why.contains(bad),
                "{bad}: {why}"
            );
        }
        assert_eq!(LegStyle::LimitIoc.word(), "limit-ioc");
    }

    #[test]
    fn the_tick_is_read_from_the_exchange_info_body_or_is_absent() {
        let with = MiniExchange::new(Knobs::default())
            .exchange_info()
            .to_string();
        assert_eq!(price_tick(&with, "AEROUSDT"), Some(d("0.0001")));
        assert_eq!(price_tick(&with, "ETHUSDT"), None, "another symbol");
        let without = MiniExchange::new(Knobs {
            no_price_filter: true,
            ..Knobs::default()
        })
        .exchange_info()
        .to_string();
        assert_eq!(price_tick(&without, "AEROUSDT"), None);
        assert_eq!(price_tick("not json", "AEROUSDT"), None);
        let zero = with.replace("0.00010000", "0.00000000");
        assert_eq!(
            price_tick(&zero, "AEROUSDT"),
            None,
            "a tick of zero prices nothing"
        );
    }

    // --- dry run, record mode ---

    #[tokio::test]
    async fn a_dry_run_prints_the_reads_that_start_the_run_and_sends_none() {
        let lab = Lab::new("dry", Knobs::default())
            .await
            .with("VP_DRY_RUN", "1");
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        let played = play_with(&lab, &run, &env, settings(1)).await;

        assert!(
            lab.received().await.is_empty(),
            "{:?}",
            lab.received().await
        );
        let printed = run.gate().printed();
        for call in [
            "DRY RUN GET /api/v3/exchangeInfo symbol=AEROUSDT",
            "DRY RUN GET /api/v3/account/commission symbol=AEROUSDT",
            "DRY RUN GET /api/v3/account omitZeroBalances=true",
        ] {
            assert!(
                printed.iter().any(|line| line == call),
                "{call} in {printed:#?}"
            );
        }
        assert!(!printed.iter().any(|line| line.contains("POST")));
        assert!(played.lines.iter().all(|line| line["verdict"] == "skipped"));
        assert!(played.clean);
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn record_mode_names_each_reply_for_its_leg_and_redacts_the_uid() {
        let lab = Lab::new("record", Knobs::default()).await;
        let record = lab.setup.record.display().to_string();
        let lab = lab.with("VP_RECORD_DIR", &record);
        let played = play(&lab, settings(1)).await;
        assert!(played.clean);

        let dir = &lab.setup.record;
        for file in [
            "00-exchange-info",
            "01-account",
            "02-account-commission",
            "01b-book",
            "01b-account-before",
            "01b-order",
            "01b-order-status",
            "01b-my-trades",
            "01b-public-trades",
            "01b-account-after",
            "01s-order",
            "99-account-after",
        ] {
            assert!(dir.join(format!("{file}.json")).exists(), "{file}");
            assert!(dir.join(format!("{file}.status")).exists(), "{file}");
        }
        let account = std::fs::read_to_string(dir.join("01-account.json")).unwrap();
        assert!(account.contains("REDACTED-UID") && !account.contains("354937868"));
        assert_eq!(played.orders()[0]["body_file"], "01b-order.json");
        lab.setup.clean_up();
    }

    // --- the test a person runs ---

    /// LV2a against Binance spot production:
    ///
    /// ```text
    /// VP_DRY_RUN=1 cargo test --lib production_lv2a_exchange -- --ignored --nocapture
    /// cargo test --lib production_lv2a_exchange -- --ignored --nocapture
    /// ```
    ///
    /// Needs `BINANCE_BASE_URL=https://api.binance.com`, `BINANCE_API_KEY` and
    /// `BINANCE_API_SECRET` (trading enabled, withdrawals off, "pay fees with BNB"
    /// off), `VP_SPEND_CAP_USD`, `VP_HALT_FILE` and `VP_OUT`; optionally `VP_SYMBOL`,
    /// `VP_ORDER_USD`, `VP_ROUND_TRIPS` (default 10; fewer for a pilot),
    /// `VP_ORDER_CAP_USD` and `VP_RECORD_DIR`.
    #[tokio::test]
    #[ignore = "places real orders on Binance production with a real key: see specs/V7-production-validation.md"]
    async fn production_lv2a_exchange() {
        let summary = crate::production::run(
            crate::production::Case::Lv2aExchange,
            &crate::production::Variables::process(),
        )
        .await
        .expect("the run");
        assert!(summary.is_clean(), "LV2a is clean: {summary:?}");
    }
}
