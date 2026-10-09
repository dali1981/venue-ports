//! LV1 against Binance spot (`specs/V7-production-validation.md`, "LV1"): the
//! requests the venue must refuse, and the reads a production run needs. One
//! test, `production_lv1_binance`, runs the cases in order; each case is a
//! method here, and each has a self-test that runs the same method against a
//! mock standing in for the venue, once with the reply the case expects and
//! once with another.
//!
//! | # | Request | Expected |
//! |---|---|---|
//! | B1 | a signed endpoint with no signature | refused (documented `-1102`) |
//! | B2 | a wrong signature | refused (documented `-1022`) |
//! | B3 | a timestamp 10 s old | refused `-1021` |
//! | B4 | a key from a host that is not whitelisted | refused `-2015`, with `VP_EXPECT_NOT_WHITELISTED=1` |
//! | B5 | an order test with a key that may not trade | refused (documented `-2015`) |
//! | B6 | an order under the minimum notional; a quantity off the lot step | refused `-1013` |
//! | B7 | a market order above an empty account's free balance | refused `-2010` |
//! | B8 | an unknown symbol | refused (documented `-1121`) |
//! | B9 | `test_order` with commission rates | accepted, the rates read |
//! | B10 | the reads: rules, balances, commission, key restrictions, book | answered; `status == "TRADING"`, `enableWithdrawals == false`, `ipRestrict == true` |
//!
//! **B5 sends an order *test*, not an order.** A key that should not trade but
//! can would otherwise place a real order; the test order is refused for the
//! same permission and costs nothing if it is not. (The refusal is recorded as
//! the venue answered it; whether a real order from such a key is refused with
//! the same code is not shown here.)
//!
//! **B6's lot-step order is `k` steps plus half a step**, sized to the order's
//! notional, not half a step on its own: a quantity under the minimum
//! quantity would fail the lot-size filter for that reason, and the case is
//! about the step.
//!
//! **Every order is under `VP_ORDER_CAP_USD`**, B7's included, and every order
//! goes through the ledger before it is signed. A request that must be refused
//! and is **accepted** is a `fail`, its notional is booked as a loss, and the
//! run stops at once.
//!
//! A dry run makes no read, so a case whose request is sized from a read (an
//! order) is skipped with that reason; the reads and the requests that need no
//! read are printed.

use crate::balance::{SpotAccountBalances, SpotBalanceReader};
use crate::cex::BinanceRest;
use crate::cex::{
    new_client_order_id, BinanceLive, CexTimings, OrderRequest, OrderSide, SymbolRules,
};
use crate::production::env::{flag, positive_decimal, Env};
use crate::production::ledger::Stop;
use crate::production::results::{Expected, Tally, Verdict};
use crate::production::sizing::{is_on_step, quantity_for, round_down};
use crate::production::{CallSpec, Called, Run};
use anyhow::{anyhow, Result};
use reqwest::Method;
use rust_decimal::Decimal;
use std::collections::HashMap;

const ACCOUNT_PATH: &str = "/api/v3/account";

/// What the run is pointed at, from the environment.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// `VP_SYMBOL`, default `AEROUSDT`.
    pub(crate) symbol: String,
    /// `VP_ORDER_USD`, default 10: about what one order is worth.
    pub(crate) order_usd: Decimal,
    /// `VP_EMPTY_MAX_USD`, default 5: the most the empty account may hold of
    /// the quote asset for B7 to run.
    pub(crate) empty_max_usd: Decimal,
    /// `VP_EXPECT_NOT_WHITELISTED=1`: this run is from a host the key does not
    /// allow, so B4 can run.
    pub(crate) expect_not_whitelisted: bool,
}

impl Settings {
    pub(crate) fn from_env(env: &dyn Env) -> Result<Self> {
        Ok(Self {
            symbol: env
                .var("VP_SYMBOL")
                .unwrap_or_else(|| "AEROUSDT".to_string()),
            order_usd: positive_decimal(env, "VP_ORDER_USD", Some(Decimal::from(10)))?,
            empty_max_usd: positive_decimal(env, "VP_EMPTY_MAX_USD", Some(Decimal::from(5)))?,
            expect_not_whitelisted: flag(env, "VP_EXPECT_NOT_WHITELISTED"),
        })
    }
}

/// Whether the run goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Go,
    Stop,
}

fn flow<T>(called: &Called<T>) -> Flow {
    if called.stops_the_run() {
        Flow::Stop
    } else {
        Flow::Go
    }
}

pub(crate) struct Lv1Binance<'a> {
    run: &'a Run,
    env: &'a dyn Env,
    allowed_host: String,
    timings: CexTimings,
    settings: Settings,
    /// The symbol's rules, once B10's read of them has answered.
    rules: Option<SymbolRules>,
    /// The trading account's balances, once read, for the exit check.
    before: Option<SpotAccountBalances>,
}

impl<'a> Lv1Binance<'a> {
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
            before: None,
        }
    }

    fn rest(&self, prefix: &str) -> Result<BinanceRest> {
        self.run
            .binance_rest(self.env, &self.allowed_host, prefix, self.timings.clone())
    }

    /// The cases, in order. Stops at once when a request that must be refused
    /// was accepted, or the run was halted.
    pub(crate) async fn run_all(&mut self) -> Result<Tally> {
        macro_rules! go {
            ($case:expr) => {
                if $case.await? == Flow::Stop {
                    return Ok(self.run.tally());
                }
            };
        }
        // From a host the key does not allow, every signed call is refused for
        // the host, which says nothing about the other cases: only B4 runs.
        if self.settings.expect_not_whitelisted {
            go!(self.b4());
            return Ok(self.run.tally());
        }
        go!(self.b10_rules());
        go!(self.b10_balances_before());
        go!(self.b1());
        go!(self.b2());
        go!(self.b3());
        go!(self.b4());
        go!(self.b5());
        go!(self.b6_notional());
        go!(self.b6_lot_step());
        go!(self.b7());
        go!(self.b8());
        go!(self.b9());
        go!(self.b10_commission());
        go!(self.b10_restrictions());
        go!(self.b10_book_ticker());
        go!(self.b10_order_book());
        go!(self.exit_balances());
        Ok(self.run.tally())
    }

    // --- what a case needs of the venue before it can be sized ---

    /// The venue's best ask for the symbol, from a read that is its own line of
    /// the case's results. `None` when the read did not answer (a dry run
    /// among the reasons).
    async fn ask(&self, case: &str) -> Result<Option<Decimal>> {
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call(CallSpec::new(case, Expected::ok()), || async {
                rest.book_ticker(&self.settings.symbol).await
            })
            .await?;
        Ok(called.result.ok().map(|ticker| ticker.ask_price))
    }

    fn skip(&self, case: &str, expected: Expected, why: &str) -> Result<Flow> {
        self.run.note(case, expected, Verdict::Skipped, why)?;
        Ok(Flow::Go)
    }

    fn ledger_stop(&self, case: &str, expected: Expected, stop: Stop) -> Result<Flow> {
        self.run
            .note(case, expected, Verdict::Halted, &stop.to_string())?;
        Ok(Flow::Stop)
    }

    /// An order the case expected the venue to refuse, and it did not, is
    /// money spent: booked as a loss of its whole notional (the case has no
    /// sell leg), so the ledger's figure can only be too high.
    fn book_accepted<T>(&self, called: &Called<T>, notional: Decimal) {
        if called.stops_the_run() && called.result.is_ok() {
            let _ = self.run.ledger().record_loss(notional);
        }
    }

    // --- B10, the reads that come first ---

    /// B10: the symbol's rules. `status == "TRADING"` is asserted.
    pub(crate) async fn b10_rules(&mut self) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let symbol = self.settings.symbol.clone();
        let called = self
            .run
            .call_with(
                CallSpec::new("B10", Expected::ok()).record_as("00-exchange-info"),
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
        let go = flow(&called);
        self.rules = called.result.ok();
        Ok(go)
    }

    /// B10: the trading account's balances, which the exit check compares the
    /// last read against.
    pub(crate) async fn b10_balances_before(&mut self) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call(
                CallSpec::new("B10", Expected::ok()).record_as("01-account"),
                || async { rest.balances().await },
            )
            .await?;
        let go = flow(&called);
        self.before = called.result.ok();
        Ok(go)
    }

    // --- B1 to B3: a request signed wrongly ---

    /// B1: a signed endpoint with no signature.
    pub(crate) async fn b1(&self) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call(
                CallSpec::new("B1", Expected::refused(&[-1102])).record_as("08-no-signature"),
                || async {
                    rest.client()
                        .signed_without_signature::<serde_json::Value>(
                            Method::GET,
                            ACCOUNT_PATH,
                            &[("omitZeroBalances", "true".to_string())],
                        )
                        .await
                        .map_err(anyhow::Error::from)
                },
            )
            .await?;
        Ok(flow(&called))
    }

    /// B2: a wrong signature, one byte of it changed.
    pub(crate) async fn b2(&self) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call(
                CallSpec::new("B2", Expected::refused(&[-1022])).record_as("09-wrong-signature"),
                || async {
                    rest.client()
                        .signed_with_corrupted_signature::<serde_json::Value>(
                            Method::GET,
                            ACCOUNT_PATH,
                            &[("omitZeroBalances", "true".to_string())],
                        )
                        .await
                        .map_err(anyhow::Error::from)
                },
            )
            .await?;
        Ok(flow(&called))
    }

    /// B3: a timestamp ten seconds behind the venue's clock, which is read first.
    /// The request is not retried.
    pub(crate) async fn b3(&self) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call(
                CallSpec::new("B3", Expected::refused(&[-1021])).record_as("10-stale-timestamp"),
                || async {
                    let client = rest.client();
                    client.sync_clock().await?;
                    let stale = u64::try_from(client.venue_now_ms() - 10_000)
                        .map_err(|_| anyhow!("the venue's clock reads before the epoch"))?;
                    client
                        .signed_with_timestamp::<serde_json::Value>(
                            Method::GET,
                            ACCOUNT_PATH,
                            &[("omitZeroBalances", "true".to_string())],
                            stale,
                        )
                        .await
                        .map_err(anyhow::Error::from)
                },
            )
            .await?;
        Ok(flow(&called))
    }

    // --- B4, B5: a key that may not ---

    /// B4: a key used from a host the venue does not whitelist for it. Run from
    /// a second host with `VP_EXPECT_NOT_WHITELISTED=1`, else skipped.
    pub(crate) async fn b4(&self) -> Result<Flow> {
        let expected = Expected::refused(&[-2015]);
        if !self.settings.expect_not_whitelisted {
            return self.skip(
                "B4",
                expected,
                "VP_EXPECT_NOT_WHITELISTED is not 1: run this case from a second host",
            );
        }
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call(
                CallSpec::new("B4", expected).record_as("11-key-not-whitelisted"),
                || async { rest.balances().await },
            )
            .await?;
        Ok(flow(&called))
    }

    /// B5: an order test with a key created with trading disabled
    /// (`BINANCE_RO_*`). See the module docs for why it is a test.
    pub(crate) async fn b5(&self) -> Result<Flow> {
        let expected = Expected::refused(&[-2015]);
        if !Run::has_keys(self.env, "BINANCE_RO") {
            return self.skip(
                "B5",
                expected,
                "BINANCE_RO_API_KEY and BINANCE_RO_API_SECRET are not set",
            );
        }
        let Some(rules) = self.rules.clone() else {
            return self.skip("B5", expected, "the symbol's rules were not read");
        };
        let Some(ask) = self.ask("B5").await? else {
            return self.skip("B5", expected, "the symbol's price was not read");
        };
        let quantity = quantity_for(self.settings.order_usd, ask, rules.lot_step)?;
        let live = BinanceLive::new(
            self.rest("BINANCE_RO")?,
            HashMap::from([(self.settings.symbol.clone(), rules.lot_step)]),
        );
        let request = OrderRequest {
            symbol: self.settings.symbol.clone(),
            side: OrderSide::Buy,
            quantity,
            quoted_price: ask,
            reduce_only: false,
        };
        let called = self
            .run
            .call(
                CallSpec::new("B5", expected).record_as("12-order-test-read-only-key"),
                || async { live.test_order(&request).await },
            )
            .await?;
        Ok(flow(&called))
    }

    // --- B6: orders the filters refuse ---

    /// B6: an order under the symbol's minimum notional (0.8 of it), which
    /// applies to market orders.
    pub(crate) async fn b6_notional(&self) -> Result<Flow> {
        let expected = Expected::refused(&[-1013]);
        let Some(rules) = self.rules.clone() else {
            return self.skip("B6", expected, "the symbol's rules were not read");
        };
        let Some(min_notional) = rules
            .min_notional
            .filter(|_| rules.apply_min_to_market == Some(true))
        else {
            return self.skip(
                "B6",
                expected,
                "the symbol has no minimum notional that the venue says applies to market \
                 orders, so an order under it might be accepted",
            );
        };
        let Some(ask) = self.ask("B6").await? else {
            return self.skip("B6", expected, "the symbol's price was not read");
        };
        let target = min_notional * Decimal::new(8, 1);
        let quantity = quantity_for(target, ask, rules.lot_step)?;
        if quantity.is_zero() {
            return self.skip(
                "B6",
                expected,
                "a notional under the minimum buys less than one lot step",
            );
        }
        self.refused_order("B6", expected, "06-order-notional", quantity, ask)
            .await
    }

    /// B6: an order whose quantity is a whole number of steps plus half a step,
    /// and whose notional is the run's order size.
    pub(crate) async fn b6_lot_step(&self) -> Result<Flow> {
        let expected = Expected::refused(&[-1013]);
        let Some(rules) = self.rules.clone() else {
            return self.skip("B6", expected, "the symbol's rules were not read");
        };
        let Some(ask) = self.ask("B6").await? else {
            return self.skip("B6", expected, "the symbol's price was not read");
        };
        let whole = quantity_for(self.settings.order_usd, ask, rules.lot_step)?;
        let quantity = whole + rules.lot_step / Decimal::TWO;
        if is_on_step(quantity, rules.lot_step) {
            return self.skip(
                "B6",
                expected,
                "the lot step cannot be halved into a quantity that is off it",
            );
        }
        self.refused_order("B6", expected, "06-order-lot-size-off-step", quantity, ask)
            .await
    }

    /// Places a market buy of `quantity` that the venue must refuse. The ledger
    /// is asked first.
    async fn refused_order(
        &self,
        case: &str,
        expected: Expected,
        stem: &str,
        quantity: Decimal,
        price: Decimal,
    ) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let permit = match self.run.authorise_order(quantity, price) {
            Ok(permit) => permit,
            Err(stop) => return self.ledger_stop(case, expected, stop),
        };
        let client_order_id = new_client_order_id();
        let called = self
            .run
            .call(CallSpec::new(case, expected).record_as(stem), || async {
                rest.place_market_order(&self.settings.symbol, "BUY", quantity, &client_order_id)
                    .await
                    .map_err(anyhow::Error::from)
            })
            .await?;
        drop(permit);
        self.book_accepted(&called, quantity * price);
        Ok(flow(&called))
    }

    // --- B7: an empty wallet ---

    /// B7: a market buy above the free balance of an empty account
    /// (`BINANCE_EMPTY_*`), run only after the account's free quote balance has
    /// been read as under `VP_EMPTY_MAX_USD`.
    pub(crate) async fn b7(&self) -> Result<Flow> {
        let expected = Expected::refused(&[-2010]);
        if !Run::has_keys(self.env, "BINANCE_EMPTY") {
            return self.skip(
                "B7",
                expected,
                "BINANCE_EMPTY_API_KEY and BINANCE_EMPTY_API_SECRET are not set",
            );
        }
        let Some(rules) = self.rules.clone() else {
            return self.skip("B7", expected, "the symbol's rules were not read");
        };
        if self.settings.empty_max_usd >= self.settings.order_usd {
            return self.skip(
                "B7",
                expected,
                "VP_EMPTY_MAX_USD is not under the order's size, so the order might be affordable",
            );
        }
        let empty = self.rest("BINANCE_EMPTY")?;
        let account = self
            .run
            .call(
                CallSpec::new("B7", Expected::ok()).record_as("07-empty-account"),
                || async { empty.balances().await },
            )
            .await?;
        let Ok(account) = account.result else {
            return self.skip("B7", expected, "the empty account's balances were not read");
        };
        let free = account
            .balance(&rules.quote_asset)
            .map_or(Decimal::ZERO, |balance| balance.free);
        if free >= self.settings.empty_max_usd {
            return self.skip(
                "B7",
                expected,
                &format!(
                    "the account holds {free} {}, not under VP_EMPTY_MAX_USD ({}): it is not empty",
                    rules.quote_asset, self.settings.empty_max_usd
                ),
            );
        }
        let Some(ask) = self.ask("B7").await? else {
            return self.skip("B7", expected, "the symbol's price was not read");
        };
        let quantity = quantity_for(self.settings.order_usd, ask, rules.lot_step)?;
        let permit = match self.run.authorise_order(quantity, ask) {
            Ok(permit) => permit,
            Err(stop) => return self.ledger_stop("B7", expected, stop),
        };
        let client_order_id = new_client_order_id();
        let called = self
            .run
            .call(
                CallSpec::new("B7", expected).record_as("07-order-insufficient-balance"),
                || async {
                    empty
                        .place_market_order(
                            &self.settings.symbol,
                            "BUY",
                            quantity,
                            &client_order_id,
                        )
                        .await
                        .map_err(anyhow::Error::from)
                },
            )
            .await?;
        drop(permit);
        self.book_accepted(&called, quantity * ask);
        Ok(flow(&called))
    }

    // --- B8: a symbol that does not exist ---

    /// B8: the book of a symbol that does not exist.
    pub(crate) async fn b8(&self) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call(
                CallSpec::new("B8", Expected::refused(&[-1121])).record_as("13-unknown-symbol"),
                || async { rest.book_ticker("NOSUCHXXXUSDT").await },
            )
            .await?;
        Ok(flow(&called))
    }

    // --- B9: an order test that is accepted ---

    /// B9: `BinanceLive::test_order` for a valid order of about the run's order
    /// size: accepted, with the commission rates the venue states.
    pub(crate) async fn b9(&self) -> Result<Flow> {
        let expected = Expected::ok();
        let Some(rules) = self.rules.clone() else {
            return self.skip("B9", expected, "the symbol's rules were not read");
        };
        if rules
            .min_notional
            .is_some_and(|min| self.settings.order_usd < min)
        {
            return self.skip(
                "B9",
                expected,
                "VP_ORDER_USD is under the symbol's minimum notional",
            );
        }
        let Some(ask) = self.ask("B9").await? else {
            return self.skip("B9", expected, "the symbol's price was not read");
        };
        let quantity = round_down(
            self.settings
                .order_usd
                .checked_div(ask)
                .ok_or_else(|| anyhow!("the order size over the price cannot be computed"))?,
            rules.lot_step,
        )?;
        let live = BinanceLive::new(
            self.rest("BINANCE")?,
            HashMap::from([(self.settings.symbol.clone(), rules.lot_step)]),
        );
        let request = OrderRequest {
            symbol: self.settings.symbol.clone(),
            side: OrderSide::Buy,
            quantity,
            quoted_price: ask,
            reduce_only: false,
        };
        let called = self
            .run
            .call_with(
                CallSpec::new("B9", expected).record_as("03-order-test-buy"),
                || async { live.test_order(&request).await },
                |_| None,
                |result| match result {
                    Ok(check) if check.rates.standard.taker < Decimal::ZERO => {
                        Err("the venue states a negative taker rate".to_string())
                    }
                    _ => Ok(()),
                },
            )
            .await?;
        Ok(flow(&called))
    }

    // --- B10: the reads ---

    /// B10: the commission the account pays on the symbol.
    pub(crate) async fn b10_commission(&self) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call(
                CallSpec::new("B10", Expected::ok()).record_as("02-account-commission"),
                || async { rest.account_commission(&self.settings.symbol).await },
            )
            .await?;
        Ok(flow(&called))
    }

    /// B10: what the trading key may do. `enableWithdrawals == false` and
    /// `ipRestrict == true` are asserted.
    pub(crate) async fn b10_restrictions(&self) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call_with(
                CallSpec::new("B10", Expected::ok()).record_as("14-api-restrictions"),
                || async { rest.api_restrictions().await },
                |_| None,
                |result| {
                    let Ok(restrictions) = result else {
                        return Ok(());
                    };
                    let mut faults = Vec::new();
                    if restrictions.enable_withdrawals {
                        faults.push("enableWithdrawals is true: the key can withdraw");
                    }
                    if !restrictions.ip_restrict {
                        faults.push("ipRestrict is false: the key works from any address");
                    }
                    if faults.is_empty() {
                        Ok(())
                    } else {
                        Err(faults.join("; "))
                    }
                },
            )
            .await?;
        Ok(flow(&called))
    }

    /// B10: the top of the book.
    pub(crate) async fn b10_book_ticker(&self) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call(
                CallSpec::new("B10", Expected::ok()).record_as("15-book-ticker"),
                || async { rest.book_ticker(&self.settings.symbol).await },
            )
            .await?;
        Ok(flow(&called))
    }

    /// B10: five levels of the book.
    pub(crate) async fn b10_order_book(&self) -> Result<Flow> {
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call(
                CallSpec::new("B10", Expected::ok()).record_as("16-order-book"),
                || async { rest.order_book(&self.settings.symbol, 5).await },
            )
            .await?;
        Ok(flow(&called))
    }

    // --- the exit of LV1 ---

    /// The account's balances after the cases are the balances before them, to
    /// the unit: nothing in LV1 may have traded.
    pub(crate) async fn exit_balances(&self) -> Result<Flow> {
        let expected = Expected::ok();
        let Some(before) = self.before.clone() else {
            return self.skip(
                "EXIT",
                expected,
                "the balances before the cases were not read, so there is nothing to compare",
            );
        };
        let rest = self.rest("BINANCE")?;
        let called = self
            .run
            .call_with(
                CallSpec::new("EXIT", expected).record_as("17-account-after"),
                || async { rest.balances().await },
                |_| None,
                |result| match result {
                    Ok(after) => same_holdings(&before, after),
                    Err(_) => Ok(()),
                },
            )
            .await?;
        Ok(flow(&called))
    }
}

/// Whether two reads of an account hold the same, asset by asset, free and
/// locked, to the unit. An asset one lacks is held at zero.
fn same_holdings(before: &SpotAccountBalances, after: &SpotAccountBalances) -> Result<(), String> {
    let mut assets: Vec<&str> = before
        .balances
        .iter()
        .chain(&after.balances)
        .map(|balance| balance.asset.as_str())
        .collect();
    assets.sort_unstable();
    assets.dedup();
    let held = |account: &SpotAccountBalances, asset: &str| {
        account
            .balance(asset)
            .map_or((Decimal::ZERO, Decimal::ZERO), |b| (b.free, b.locked))
    };
    let changes: Vec<String> = assets
        .into_iter()
        .filter_map(|asset| {
            let (was, is) = (held(before, asset), held(after, asset));
            (was != is).then(|| {
                format!(
                    "{asset}: free {} -> {}, locked {} -> {}",
                    was.0, is.0, was.1, is.1
                )
            })
        })
        .collect();
    if changes.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the account changed during LV1: {}",
            changes.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::binance::clock::local_now_ms;
    use crate::cex::binance::sign;
    use crate::production::env::ProcessEnv;
    use crate::production::gate::Echo;
    use crate::production::guard::PRODUCTION_BINANCE_SPOT;
    use crate::production::testing::{fast, Setup};
    use serde_json::{json, Value};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn d(text: &str) -> Decimal {
        Decimal::from_str(text).unwrap()
    }

    // The three keys of a run. `Setup` gives the trading key; the read-only and
    // the empty account's are added where a case needs them.
    const TRADER: (&str, &str) = ("AKEY-1234-ABCD", "SECRET-9876-WXYZ");
    const READ_ONLY: (&str, &str) = ("ROKEY-1111-ABCD", "ROSECRET-2222-WXYZ");
    const EMPTY: (&str, &str) = ("EMPTYKEY-3333-ABCD", "EMPTYSECRET-4444-WXYZ");

    const BID: &str = "1.0000";
    const ASK: &str = "1.0010";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Role {
        Trader,
        ReadOnly,
        Empty,
    }

    /// A stand-in for Binance spot that checks what Binance checks of these
    /// requests: the key, the signature, the timestamp against `recvWindow`, the
    /// key's permission, the lot step, the minimum notional and the balance. The
    /// bodies of its refusals are the documented codes and messages
    /// (`errors.md` of Binance's `binance-spot-api-docs`); the HTTP status of
    /// each is the venue's convention, which that page does not print. Its
    /// answers to reads are synthetic or the documentation's, as the catalogue
    /// says.
    ///
    /// Its knobs make it a venue that misbehaves: one that accepts what it must
    /// refuse, one that refuses with a code no row knows, one that is not what
    /// the key allows.
    #[derive(Clone)]
    struct MiniVenue {
        /// The caller's address is whitelisted for the key.
        whitelisted: bool,
        /// Accepts every request: no signature, key, timestamp, filter or
        /// balance is checked, and a symbol that does not exist is a symbol.
        accepts_everything: bool,
        /// Every refusal carries this code instead of its own.
        refusal_code: Option<i64>,
        status: &'static str,
        withdrawals: bool,
        ip_restrict: bool,
        refuses_order_tests: bool,
        /// The trading account holds less after its first read.
        account_changes: bool,
        trader_reads: Arc<AtomicUsize>,
        fills: Arc<AtomicUsize>,
    }

    // `ResponseTemplate` is large; a test double does not care.
    #[allow(clippy::result_large_err)]
    impl MiniVenue {
        fn documented() -> Self {
            Self {
                whitelisted: true,
                accepts_everything: false,
                refusal_code: None,
                status: "TRADING",
                withdrawals: false,
                ip_restrict: true,
                refuses_order_tests: false,
                account_changes: false,
                trader_reads: Arc::new(AtomicUsize::new(0)),
                fills: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn refuse(&self, status: u16, code: i64, msg: &str) -> ResponseTemplate {
            ResponseTemplate::new(status).set_body_json(json!({
                "code": self.refusal_code.unwrap_or(code),
                "msg": msg
            }))
        }

        fn ok(body: Value) -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(body)
        }

        fn role(key: &str) -> Option<(Role, &'static str)> {
            [
                (TRADER, Role::Trader),
                (READ_ONLY, Role::ReadOnly),
                (EMPTY, Role::Empty),
            ]
            .into_iter()
            .find(|((api_key, _), _)| *api_key == key)
            .map(|((_, secret), role)| (role, secret))
        }

        /// Who is calling, or the refusal Binance answers with.
        fn authenticate(&self, request: &Request) -> Result<Role, ResponseTemplate> {
            let key = request
                .headers
                .get("X-MBX-APIKEY")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            let Some((role, secret)) = Self::role(key) else {
                return Err(self.refuse(401, -2014, "API-key format invalid."));
            };
            if self.accepts_everything {
                return Ok(role);
            }
            if !self.whitelisted {
                return Err(self.refuse(
                    401,
                    -2015,
                    "Invalid API-key, IP, or permissions for action.",
                ));
            }
            let query = request.url.query().unwrap_or_default();
            let Some((unsigned, signature)) = query.rsplit_once("&signature=") else {
                return Err(self.refuse(
                    400,
                    -1102,
                    "Mandatory parameter 'signature' was not sent, was empty/null, or malformed.",
                ));
            };
            if sign::sign(secret, unsigned) != signature {
                return Err(self.refuse(400, -1022, "Signature for this request is not valid."));
            }
            let param = |name: &str| {
                request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == name)
                    .and_then(|(_, value)| value.parse::<i64>().ok())
            };
            let (timestamp, window) = (param("timestamp").unwrap_or(0), param("recvWindow"));
            if (local_now_ms() - timestamp).abs() > window.unwrap_or(5_000) {
                return Err(self.refuse(
                    400,
                    -1021,
                    "Timestamp for this request is outside of the recvWindow.",
                ));
            }
            Ok(role)
        }

        fn quantity(request: &Request) -> Decimal {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == "quantity")
                .and_then(|(_, value)| Decimal::from_str(&value).ok())
                .unwrap_or_default()
        }

        /// The filters of AEROUSDT: a lot step of 0.1 and a minimum notional of 5
        /// that applies to market orders.
        fn filters(&self, request: &Request) -> Result<(), ResponseTemplate> {
            if self.accepts_everything {
                return Ok(());
            }
            let quantity = Self::quantity(request);
            if quantity < d("0.1") || !is_on_step(quantity, d("0.1")) {
                return Err(self.refuse(400, -1013, "Filter failure: LOT_SIZE"));
            }
            if quantity * d(ASK) < d("5") {
                return Err(self.refuse(400, -1013, "Filter failure: NOTIONAL"));
            }
            Ok(())
        }

        fn balances(&self, role: Role) -> Value {
            let usdt = match role {
                Role::Empty => "0.50000000",
                _ if self.account_changes && self.trader_reads.load(Ordering::SeqCst) > 1 => {
                    "99.00000000"
                }
                _ => "100.00000000",
            };
            json!({
                "updateTime": 1_700_000_000_000u64,
                "accountType": "SPOT",
                "balances": [{"asset": "USDT", "free": usdt, "locked": "0.00000000"}],
                "permissions": ["SPOT"],
                "uid": 354937868
            })
        }

        fn rates() -> Value {
            json!({
                "standardCommissionForOrder": {"maker": "0.00100000", "taker": "0.00100000"},
                "specialCommissionForOrder": {"maker": "0.00000000", "taker": "0.00000000"},
                "taxCommissionForOrder": {"maker": "0.00000000", "taker": "0.00000000"},
                "discount": {"enabledForAccount": true, "enabledForSymbol": true,
                             "discountAsset": "BNB", "discount": "0.25000000"}
            })
        }

        fn exchange_info(&self) -> Value {
            json!({
                "timezone": "UTC", "serverTime": local_now_ms(), "rateLimits": [],
                "exchangeFilters": [],
                "symbols": [{
                    "symbol": "AEROUSDT", "status": self.status,
                    "baseAsset": "AERO", "quoteAsset": "USDT",
                    "filters": [
                        {"filterType": "LOT_SIZE", "minQty": "0.10000000",
                         "maxQty": "100000.00000000", "stepSize": "0.10000000"},
                        {"filterType": "NOTIONAL", "minNotional": "5.00000000",
                         "applyMinToMarket": true, "maxNotional": "9000000.00000000",
                         "applyMaxToMarket": false, "avgPriceMins": 5}
                    ]
                }]
            })
        }
    }

    impl Respond for MiniVenue {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let symbol = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "symbol")
                .map(|(_, value)| value.into_owned());
            match (request.method.as_str(), request.url.path()) {
                ("GET", "/api/v3/time") => Self::ok(json!({"serverTime": local_now_ms()})),
                ("GET", "/api/v3/exchangeInfo") => Self::ok(self.exchange_info()),
                ("GET", "/api/v3/ticker/bookTicker") => match symbol {
                    Some(symbol) if symbol == "AEROUSDT" || self.accepts_everything => {
                        Self::ok(json!({"symbol": symbol, "bidPrice": BID, "bidQty": "500.0",
                                        "askPrice": ASK, "askQty": "500.0"}))
                    }
                    _ => self.refuse(400, -1121, "Invalid symbol."),
                },
                ("GET", "/api/v3/depth") => Self::ok(json!({
                    "lastUpdateId": 1027024,
                    "bids": [["1.0000", "500.0"], ["0.9990", "300.0"]],
                    "asks": [["1.0010", "500.0"], ["1.0020", "300.0"]]
                })),
                (verb, path) => {
                    let role = match self.authenticate(request) {
                        Ok(role) => role,
                        Err(refusal) => return refusal,
                    };
                    match (verb, path) {
                        ("GET", "/api/v3/account") => {
                            if role == Role::Trader {
                                self.trader_reads.fetch_add(1, Ordering::SeqCst);
                            }
                            Self::ok(self.balances(role))
                        }
                        ("GET", "/api/v3/account/commission") => Self::ok(json!({
                            "symbol": "AEROUSDT",
                            "standardCommission": {"maker": "0.00100000", "taker": "0.00100000",
                                                   "buyer": "0.00000000", "seller": "0.00000000"},
                            "specialCommission": {"maker": "0.00000000", "taker": "0.00000000",
                                                  "buyer": "0.00000000", "seller": "0.00000000"},
                            "taxCommission": {"maker": "0.00000000", "taker": "0.00000000",
                                              "buyer": "0.00000000", "seller": "0.00000000"},
                            "discount": {"enabledForAccount": true, "enabledForSymbol": true,
                                         "discountAsset": "BNB", "discount": "0.75000000"}
                        })),
                        ("GET", "/sapi/v1/account/apiRestrictions") => Self::ok(json!({
                            "ipRestrict": self.ip_restrict, "createTime": 1_698_645_219_000u64,
                            "enableReading": true, "enableWithdrawals": self.withdrawals,
                            "enableInternalTransfer": false, "enableMargin": false,
                            "enableFutures": false, "enableSpotAndMarginTrading": true
                        })),
                        ("POST", "/api/v3/order/test") => {
                            if role == Role::ReadOnly && !self.accepts_everything {
                                return self.refuse(
                                    401,
                                    -2015,
                                    "Invalid API-key, IP, or permissions for action.",
                                );
                            }
                            if self.refuses_order_tests {
                                return self.refuse(400, -1013, "Filter failure: NOTIONAL");
                            }
                            match self.filters(request) {
                                Ok(()) => Self::ok(Self::rates()),
                                Err(refusal) => refusal,
                            }
                        }
                        ("POST", "/api/v3/order") => {
                            if role == Role::ReadOnly && !self.accepts_everything {
                                return self.refuse(
                                    401,
                                    -2015,
                                    "Invalid API-key, IP, or permissions for action.",
                                );
                            }
                            if let Err(refusal) = self.filters(request) {
                                return refusal;
                            }
                            if role == Role::Empty && !self.accepts_everything {
                                return self.refuse(
                                    400,
                                    -2010,
                                    "Account has insufficient balance for requested action.",
                                );
                            }
                            self.fills.fetch_add(1, Ordering::SeqCst);
                            let quantity = Self::quantity(request);
                            Self::ok(json!({
                                "orderId": 42, "status": "FILLED",
                                "executedQty": quantity.to_string(),
                                "transactTime": 1_700_000_000_000u64,
                                "fills": [{"price": ASK, "qty": quantity.to_string(),
                                           "commission": "0", "commissionAsset": "AERO",
                                           "tradeId": 7}]
                            }))
                        }
                        _ => ResponseTemplate::new(404),
                    }
                }
            }
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

    fn settings() -> Settings {
        Settings {
            symbol: "AEROUSDT".to_string(),
            order_usd: d("10"),
            empty_max_usd: d("5"),
            expect_not_whitelisted: false,
        }
    }

    /// A run against a mini venue, in a scratch directory, with every key set.
    struct Lab {
        server: MockServer,
        setup: Setup,
        venue: MiniVenue,
    }

    impl Lab {
        async fn new(name: &str, venue: MiniVenue) -> Self {
            let server = MockServer::start().await;
            Mock::given(any())
                .respond_with(venue.clone())
                .mount(&server)
                .await;
            let setup = Setup::new(name, &server)
                .with("BINANCE_RO_API_KEY", READ_ONLY.0)
                .with("BINANCE_RO_API_SECRET", READ_ONLY.1)
                .with("BINANCE_EMPTY_API_KEY", EMPTY.0)
                .with("BINANCE_EMPTY_API_SECRET", EMPTY.1);
            Self {
                server,
                setup,
                venue,
            }
        }

        fn with(mut self, key: &str, value: &str) -> Self {
            self.setup = self.setup.with(key, value);
            self
        }

        fn without(mut self, key: &str) -> Self {
            self.setup = self.setup.without(key);
            self
        }

        /// The calls the venue received, as `METHOD /path`.
        async fn received(&self) -> Vec<String> {
            self.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|request| format!("{} {}", request.method, request.url.path()))
                .collect()
        }

        async fn requests_to(&self, route: &str) -> Vec<Request> {
            self.server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|request| request.url.path() == route)
                .collect()
        }
    }

    /// The case run against the lab: its results lines, and whether the run goes on.
    struct Played {
        lines: Vec<Value>,
        flow: Flow,
    }

    impl Played {
        fn last(&self) -> &Value {
            self.lines.last().expect("the case wrote a line")
        }

        fn verdict(&self) -> &str {
            self.last()["verdict"].as_str().unwrap()
        }

        fn reason(&self) -> &str {
            self.last()["reason"].as_str().unwrap()
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Case {
        B1,
        B2,
        B3,
        B4,
        B5,
        B6Notional,
        B6LotStep,
        B7,
        B8,
        B9,
        Commission,
        Restrictions,
        BookTicker,
        OrderBook,
        Exit,
    }

    /// Runs one case with the rules read, and the balances read when the case
    /// compares them (the exit does).
    async fn play(lab: &Lab, case: Case, settings: Settings) -> Played {
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        let mut lv1 = Lv1Binance::new(&run, &env, lab.server.uri(), fast(), settings);
        lv1.rules = Some(rules());
        let flow = match case {
            Case::B1 => lv1.b1().await,
            Case::B2 => lv1.b2().await,
            Case::B3 => lv1.b3().await,
            Case::B4 => lv1.b4().await,
            Case::B5 => lv1.b5().await,
            Case::B6Notional => lv1.b6_notional().await,
            Case::B6LotStep => lv1.b6_lot_step().await,
            Case::B7 => lv1.b7().await,
            Case::B8 => lv1.b8().await,
            Case::B9 => lv1.b9().await,
            Case::Commission => lv1.b10_commission().await,
            Case::Restrictions => lv1.b10_restrictions().await,
            Case::BookTicker => lv1.b10_book_ticker().await,
            Case::OrderBook => lv1.b10_order_book().await,
            Case::Exit => {
                lv1.b10_balances_before().await.unwrap();
                lv1.exit_balances().await
            }
        }
        .unwrap();
        Played {
            lines: lab.setup.lines(),
            flow,
        }
    }

    // --- every case, once with the reply it expects ---

    /// B1 to B3: a request signed wrongly is refused with the documented code,
    /// and the case sent exactly the request it says it did.
    #[tokio::test]
    async fn b1_a_request_with_no_signature_is_refused_1102() {
        let lab = Lab::new("b1", MiniVenue::documented()).await;
        let played = play(&lab, Case::B1, settings()).await;
        assert_eq!((played.verdict(), played.flow), ("pass", Flow::Go));
        assert_eq!(played.last()["outcome"]["code"], -1102);
        let sent = lab.requests_to("/api/v3/account").await;
        assert_eq!(sent.len(), 1, "one request, no retry");
        assert!(!sent[0].url.query().unwrap().contains("signature"));
        assert_eq!(
            played.last()["body_file"],
            Value::Null,
            "no recording asked for"
        );
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b2_a_wrong_signature_is_refused_1022() {
        let lab = Lab::new("b2", MiniVenue::documented()).await;
        let played = play(&lab, Case::B2, settings()).await;
        assert_eq!(played.verdict(), "pass");
        assert_eq!(played.last()["outcome"]["code"], -1022);
        assert_eq!(lab.requests_to("/api/v3/account").await.len(), 1);
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b3_a_timestamp_ten_seconds_old_is_refused_1021_and_not_retried() {
        let lab = Lab::new("b3", MiniVenue::documented()).await;
        let played = play(&lab, Case::B3, settings()).await;
        assert_eq!(played.verdict(), "pass");
        assert_eq!(played.last()["outcome"]["code"], -1021);
        let sent = lab.requests_to("/api/v3/account").await;
        assert_eq!(
            sent.len(),
            1,
            "the normal path would have read the clock and retried"
        );
        let timestamp: i64 = sent[0]
            .url
            .query_pairs()
            .find(|(key, _)| key == "timestamp")
            .unwrap()
            .1
            .parse()
            .unwrap();
        let age = local_now_ms() - timestamp;
        assert!((10_000..12_000).contains(&age), "{age} ms old");
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b4_a_key_from_a_host_that_is_not_whitelisted_is_refused_2015() {
        let mut venue = MiniVenue::documented();
        venue.whitelisted = false;
        let lab = Lab::new("b4", venue).await;
        let mut flagged = settings();
        flagged.expect_not_whitelisted = true;
        let played = play(&lab, Case::B4, flagged).await;
        assert_eq!(played.verdict(), "pass");
        assert_eq!(played.last()["outcome"]["code"], -2015);
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b4_is_skipped_from_the_host_the_key_allows() {
        let lab = Lab::new("b4-skip", MiniVenue::documented()).await;
        let played = play(&lab, Case::B4, settings()).await;
        assert_eq!((played.verdict(), played.flow), ("skipped", Flow::Go));
        assert!(
            played.reason().contains("VP_EXPECT_NOT_WHITELISTED"),
            "{}",
            played.reason()
        );
        assert!(lab.received().await.is_empty(), "nothing was sent");
        lab.setup.clean_up();
    }

    /// B5 is an order test with the read-only key, and no real order is placed.
    #[tokio::test]
    async fn b5_an_order_test_with_a_key_that_may_not_trade_is_refused_2015() {
        let lab = Lab::new("b5", MiniVenue::documented()).await;
        let played = play(&lab, Case::B5, settings()).await;
        assert_eq!(played.verdict(), "pass");
        assert_eq!(played.last()["outcome"]["code"], -2015);
        assert_eq!(played.last()["request"]["path"], "/api/v3/order/test");
        let tests = lab.requests_to("/api/v3/order/test").await;
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].headers.get("X-MBX-APIKEY").unwrap(), READ_ONLY.0);
        assert!(
            lab.requests_to("/api/v3/order").await.is_empty(),
            "no real order"
        );
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b5_is_skipped_without_the_read_only_key() {
        let lab = Lab::new("b5-skip", MiniVenue::documented())
            .await
            .without("BINANCE_RO_API_KEY");
        let played = play(&lab, Case::B5, settings()).await;
        assert_eq!(played.verdict(), "skipped");
        assert!(
            played.reason().contains("BINANCE_RO_API_KEY"),
            "{}",
            played.reason()
        );
        lab.setup.clean_up();
    }

    /// B6: an order at 0.8 of the minimum notional, sized from the book, is
    /// refused for its notional by a venue that checks it.
    #[tokio::test]
    async fn b6_an_order_under_the_minimum_notional_is_refused_1013() {
        let lab = Lab::new("b6n", MiniVenue::documented()).await;
        let played = play(&lab, Case::B6Notional, settings()).await;
        assert_eq!(played.verdict(), "pass");
        assert_eq!(played.last()["outcome"]["code"], -1013);
        assert_eq!(played.last()["outcome"]["msg"], "Filter failure: NOTIONAL");
        let orders = lab.requests_to("/api/v3/order").await;
        let quantity = MiniVenue::quantity(&orders[0]);
        assert_eq!(quantity, d("3.9"), "0.8 of 5 USD at 1.0010, to the step");
        assert!(quantity * d(ASK) < d("5"));
        assert_eq!(lab.venue.fills.load(Ordering::SeqCst), 0);
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b6_a_quantity_off_the_lot_step_is_refused_1013_for_the_step_alone() {
        let lab = Lab::new("b6s", MiniVenue::documented()).await;
        let played = play(&lab, Case::B6LotStep, settings()).await;
        assert_eq!(played.verdict(), "pass");
        assert_eq!(played.last()["outcome"]["msg"], "Filter failure: LOT_SIZE");
        let quantity = MiniVenue::quantity(&lab.requests_to("/api/v3/order").await[0]);
        assert_eq!(
            quantity,
            d("9.95"),
            "9.9 is 10 USD at 1.0010 to the step, plus half a step"
        );
        assert!(!is_on_step(quantity, d("0.1")));
        assert!(
            quantity * d(ASK) >= d("5"),
            "the notional is not what fails"
        );
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b6_is_skipped_when_the_venue_does_not_say_the_minimum_applies_to_market_orders() {
        let lab = Lab::new("b6-skip", MiniVenue::documented()).await;
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        let mut lv1 = Lv1Binance::new(&run, &env, lab.server.uri(), fast(), settings());
        let mut unsure = rules();
        unsure.apply_min_to_market = None;
        lv1.rules = Some(unsure);
        lv1.b6_notional().await.unwrap();
        let lines = lab.setup.lines();
        assert_eq!(lines[0]["verdict"], "skipped");
        assert!(lab.requests_to("/api/v3/order").await.is_empty());
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b7_a_market_order_on_an_empty_account_is_refused_2010() {
        let lab = Lab::new("b7", MiniVenue::documented()).await;
        let played = play(&lab, Case::B7, settings()).await;
        assert_eq!(played.verdict(), "pass");
        assert_eq!(played.last()["outcome"]["code"], -2010);
        let orders = lab.requests_to("/api/v3/order").await;
        assert_eq!(orders[0].headers.get("X-MBX-APIKEY").unwrap(), EMPTY.0);
        assert_eq!(lab.venue.fills.load(Ordering::SeqCst), 0);
        // The free balance was read first.
        assert_eq!(played.lines[0]["request"]["path"], "/api/v3/account");
        lab.setup.clean_up();
    }

    /// An account that is not empty could afford the order: nothing is sent.
    #[tokio::test]
    async fn b7_is_skipped_when_the_account_is_not_empty() {
        let lab = Lab::new("b7-rich", MiniVenue::documented()).await;
        let mut rich_account = settings();
        rich_account.empty_max_usd = d("0.25");
        let played = play(&lab, Case::B7, rich_account).await;
        assert_eq!(played.verdict(), "skipped");
        assert!(
            played.reason().contains("not under VP_EMPTY_MAX_USD"),
            "{}",
            played.reason()
        );
        assert!(lab.requests_to("/api/v3/order").await.is_empty());
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b7_is_skipped_when_the_empty_cap_is_not_under_the_order() {
        let lab = Lab::new("b7-cap", MiniVenue::documented()).await;
        let mut loose = settings();
        loose.empty_max_usd = d("10");
        let played = play(&lab, Case::B7, loose).await;
        assert_eq!(played.verdict(), "skipped");
        assert!(lab.received().await.is_empty());
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b8_an_unknown_symbol_is_refused_1121() {
        let lab = Lab::new("b8", MiniVenue::documented()).await;
        let played = play(&lab, Case::B8, settings()).await;
        assert_eq!(played.verdict(), "pass");
        assert_eq!(played.last()["outcome"]["code"], -1121);
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b9_a_valid_order_test_is_accepted_and_its_rates_are_read() {
        let lab = Lab::new("b9", MiniVenue::documented()).await;
        let played = play(&lab, Case::B9, settings()).await;
        assert_eq!(played.verdict(), "pass");
        assert_eq!(played.last()["outcome"]["kind"], "ok");
        let sent = lab.requests_to("/api/v3/order/test").await;
        assert_eq!(MiniVenue::quantity(&sent[0]), d("9.9"));
        assert_eq!(
            sent[0]
                .url
                .query_pairs()
                .find(|(key, _)| key == "computeCommissionRates")
                .unwrap()
                .1,
            "true"
        );
        assert!(lab.requests_to("/api/v3/order").await.is_empty());
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b10_the_reads_answer() {
        for (case, route) in [
            (Case::Commission, "/api/v3/account/commission"),
            (Case::Restrictions, "/sapi/v1/account/apiRestrictions"),
            (Case::BookTicker, "/api/v3/ticker/bookTicker"),
            (Case::OrderBook, "/api/v3/depth"),
        ] {
            let lab = Lab::new("b10", MiniVenue::documented()).await;
            let played = play(&lab, case, settings()).await;
            assert_eq!(played.verdict(), "pass", "{case:?}: {}", played.reason());
            assert_eq!(played.last()["request"]["path"], route);
            lab.setup.clean_up();
        }
    }

    #[tokio::test]
    async fn the_exit_finds_the_account_as_it_was() {
        let lab = Lab::new("exit", MiniVenue::documented()).await;
        let played = play(&lab, Case::Exit, settings()).await;
        assert_eq!(played.verdict(), "pass");
        lab.setup.clean_up();
    }

    // --- every case, once with another reply ---

    /// A venue that accepts what it must refuse: the case fails, the run stops
    /// at once, and an order that filled is booked as a loss.
    #[tokio::test]
    async fn a_request_the_venue_accepts_is_a_fail_and_stops_the_run() {
        for case in [
            Case::B1,
            Case::B2,
            Case::B3,
            Case::B5,
            Case::B6Notional,
            Case::B6LotStep,
            Case::B7,
            Case::B8,
        ] {
            let mut venue = MiniVenue::documented();
            venue.accepts_everything = true;
            let lab = Lab::new("accepts", venue).await;
            let played = play(&lab, case, settings()).await;
            assert_eq!(played.verdict(), "fail", "{case:?}");
            assert!(
                played.reason().contains("accepted"),
                "{case:?}: {}",
                played.reason()
            );
            assert_eq!(played.flow, Flow::Stop, "{case:?}");
            lab.setup.clean_up();
        }
        // B4 only runs from the host the key does not allow, and a venue that
        // accepts it there fails it the same way.
        let mut venue = MiniVenue::documented();
        venue.accepts_everything = true;
        let lab = Lab::new("accepts-b4", venue).await;
        let mut flagged = settings();
        flagged.expect_not_whitelisted = true;
        let played = play(&lab, Case::B4, flagged).await;
        assert_eq!((played.verdict(), played.flow), ("fail", Flow::Stop));
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn an_order_the_venue_filled_against_expectation_is_booked_as_a_loss() {
        let mut venue = MiniVenue::documented();
        venue.accepts_everything = true;
        let lab = Lab::new("booked", venue).await;
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        let mut lv1 = Lv1Binance::new(&run, &env, lab.server.uri(), fast(), settings());
        lv1.rules = Some(rules());
        lv1.b6_lot_step().await.unwrap();
        assert_eq!(lab.venue.fills.load(Ordering::SeqCst), 1);
        // 9.95 AERO at 1.0010 is 9.96 USD at stake.
        assert_eq!(run.ledger().loss().unwrap(), d("9.95") * d(ASK));
        lab.setup.clean_up();
    }

    /// A refusal with a code no row has is neither a pass nor a fail.
    #[tokio::test]
    async fn a_refusal_with_a_code_no_row_has_is_unmapped_and_the_run_goes_on() {
        for case in [
            Case::B1,
            Case::B2,
            Case::B3,
            Case::B5,
            Case::B6Notional,
            Case::B6LotStep,
            Case::B7,
            Case::B8,
        ] {
            let mut venue = MiniVenue::documented();
            venue.refusal_code = Some(-9999);
            let lab = Lab::new("unmapped", venue).await;
            let played = play(&lab, case, settings()).await;
            assert_eq!(
                played.verdict(),
                "unmapped",
                "{case:?}: {}",
                played.reason()
            );
            assert!(
                played.reason().contains("-9999"),
                "{case:?}: {}",
                played.reason()
            );
            assert_eq!(played.flow, Flow::Go, "{case:?}");
            lab.setup.clean_up();
        }
    }

    #[tokio::test]
    async fn b4_from_the_allowed_host_is_accepted_and_that_is_a_fail() {
        // The key is allowed from here: the venue answers the read, and the case
        // that expected -2015 says so.
        let lab = Lab::new("b4-allowed", MiniVenue::documented()).await;
        let mut flagged = settings();
        flagged.expect_not_whitelisted = true;
        let played = play(&lab, Case::B4, flagged).await;
        assert_eq!((played.verdict(), played.flow), ("fail", Flow::Stop));
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b9_fails_when_the_order_test_is_refused() {
        let mut venue = MiniVenue::documented();
        venue.refuses_order_tests = true;
        let lab = Lab::new("b9-refused", venue).await;
        let played = play(&lab, Case::B9, settings()).await;
        assert_eq!(played.verdict(), "fail");
        assert!(played.reason().contains("-1013"), "{}", played.reason());
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b10_asserts_trading_no_withdrawals_and_an_ip_restriction() {
        let mut venue = MiniVenue::documented();
        venue.withdrawals = true;
        venue.ip_restrict = false;
        let lab = Lab::new("b10-key", venue).await;
        let played = play(&lab, Case::Restrictions, settings()).await;
        assert_eq!(played.verdict(), "fail");
        assert!(
            played.reason().contains("enableWithdrawals is true"),
            "{}",
            played.reason()
        );
        assert!(
            played.reason().contains("ipRestrict is false"),
            "{}",
            played.reason()
        );
        lab.setup.clean_up();

        let mut venue = MiniVenue::documented();
        venue.withdrawals = true;
        let lab = Lab::new("b10-withdraw", venue).await;
        let played = play(&lab, Case::Restrictions, settings()).await;
        assert_eq!(played.verdict(), "fail");
        assert!(!played.reason().contains("ipRestrict"));
        lab.setup.clean_up();

        let mut venue = MiniVenue::documented();
        venue.ip_restrict = false;
        let lab = Lab::new("b10-ip", venue).await;
        let played = play(&lab, Case::Restrictions, settings()).await;
        assert_eq!(played.verdict(), "fail");
        assert!(
            played.reason().contains("ipRestrict is false"),
            "{}",
            played.reason()
        );
        assert!(!played.reason().contains("enableWithdrawals"));
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn b10_asserts_the_symbol_is_trading() {
        let mut venue = MiniVenue::documented();
        venue.status = "BREAK";
        let lab = Lab::new("b10-status", venue).await;
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        let mut lv1 = Lv1Binance::new(&run, &env, lab.server.uri(), fast(), settings());
        lv1.b10_rules().await.unwrap();
        let line = lab.setup.lines().pop().unwrap();
        assert_eq!(line["verdict"], "fail");
        assert!(
            line["reason"].as_str().unwrap().contains("\"BREAK\""),
            "{}",
            line["reason"]
        );
        // The rules are kept all the same: the cases that size from them go on.
        assert_eq!(lv1.rules.as_ref().unwrap().status, "BREAK");
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn the_exit_fails_when_the_account_changed() {
        let mut venue = MiniVenue::documented();
        venue.account_changes = true;
        let lab = Lab::new("exit-changed", venue).await;
        let played = play(&lab, Case::Exit, settings()).await;
        assert_eq!(played.verdict(), "fail");
        assert!(
            played.reason().contains("USDT: free 100"),
            "{}",
            played.reason()
        );
        assert!(played.reason().contains("-> 99"), "{}", played.reason());
        lab.setup.clean_up();
    }

    #[tokio::test]
    async fn the_exit_is_skipped_when_there_is_nothing_to_compare_with() {
        let lab = Lab::new("exit-none", MiniVenue::documented()).await;
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        let lv1 = Lv1Binance::new(&run, &env, lab.server.uri(), fast(), settings());
        lv1.exit_balances().await.unwrap();
        assert_eq!(lab.setup.lines()[0]["verdict"], "skipped");
        assert!(lab.received().await.is_empty());
        lab.setup.clean_up();
    }

    #[test]
    fn two_reads_of_an_account_are_the_same_only_to_the_unit() {
        let account = |free: &str, locked: &str, extra: bool| SpotAccountBalances {
            balances: [("USDT", free, locked)]
                .into_iter()
                .chain(extra.then_some(("BNB", "1", "0")))
                .map(|(asset, free, locked)| crate::balance::SpotBalance {
                    asset: asset.to_string(),
                    free: d(free),
                    locked: d(locked),
                })
                .collect(),
            update_time_ms: None,
        };
        assert!(same_holdings(&account("5", "0", false), &account("5.00", "0.0", false)).is_ok());
        assert!(same_holdings(
            &account("5", "0", false),
            &account("5.00000001", "0", false)
        )
        .is_err());
        assert!(same_holdings(&account("5", "0", false), &account("5", "1", false)).is_err());
        let gained =
            same_holdings(&account("5", "0", false), &account("5", "0", true)).unwrap_err();
        assert!(gained.contains("BNB: free 0 -> 1"), "{gained}");
        // An asset that is absent is held at zero.
        assert!(same_holdings(
            &account("0", "0", false),
            &SpotAccountBalances {
                balances: vec![],
                update_time_ms: None
            }
        )
        .is_ok());
    }

    // --- the whole of LV1 ---

    /// Against a venue that behaves as documented, every case passes or is
    /// skipped for a reason, no order fills, and the recording has a file for
    /// each named call.
    #[tokio::test]
    async fn the_whole_of_lv1_is_clean_against_a_venue_that_behaves_as_documented() {
        let record = Setup::new("probe", &MockServer::start().await)
            .record
            .display()
            .to_string();
        let lab = Lab::new("whole", MiniVenue::documented())
            .await
            .with("VP_RECORD_DIR", &record);
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        let mut lv1 = Lv1Binance::new(&run, &env, lab.server.uri(), fast(), settings());

        let tally = lv1.run_all().await.unwrap();

        let lines = lab.setup.lines();
        let verdicts: Vec<(&str, &str)> = lines
            .iter()
            .map(|line| {
                (
                    line["case"].as_str().unwrap(),
                    line["verdict"].as_str().unwrap(),
                )
            })
            .collect();
        assert!(
            verdicts
                .iter()
                .all(|(_, v)| matches!(*v, "pass" | "skipped")),
            "{verdicts:?}"
        );
        assert_eq!(
            verdicts
                .iter()
                .filter(|(_, v)| *v == "skipped")
                .collect::<Vec<_>>(),
            [&("B4", "skipped")],
            "only B4, which needs a second host, is skipped"
        );
        assert_eq!(tally.fail + tally.unmapped + tally.halted, 0);
        assert!(run.finish().is_ok());
        assert_eq!(lab.venue.fills.load(Ordering::SeqCst), 0, "no order filled");
        for case in [
            "B1", "B2", "B3", "B5", "B6", "B7", "B8", "B9", "B10", "EXIT",
        ] {
            assert!(
                verdicts.iter().any(|(c, _)| *c == case),
                "{case} wrote no line"
            );
        }
        // The recording has one file for each named call, the uid redacted.
        let mut recorded: Vec<String> = std::fs::read_dir(&record)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".json") && !name.starts_with("unnamed"))
            .collect();
        recorded.sort();
        assert_eq!(
            recorded,
            [
                "00-exchange-info.json",
                "01-account.json",
                "02-account-commission.json",
                "03-order-test-buy.json",
                "06-order-lot-size-off-step.json",
                "06-order-notional.json",
                "07-empty-account.json",
                "07-order-insufficient-balance.json",
                "08-no-signature.json",
                "09-wrong-signature.json",
                "10-stale-timestamp.json",
                "12-order-test-read-only-key.json",
                "13-unknown-symbol.json",
                "14-api-restrictions.json",
                "15-book-ticker.json",
                "16-order-book.json",
                "17-account-after.json",
            ]
        );
        let account = std::fs::read_to_string(format!("{record}/01-account.json")).unwrap();
        assert!(account.contains("\"uid\":\"REDACTED-UID\""), "{account}");
        assert!(!account.contains("354937868"));
        // 08-no-signature is the refusal's own body and status.
        assert_eq!(
            std::fs::read_to_string(format!("{record}/08-no-signature.status")).unwrap(),
            "400\n"
        );
        lab.setup.clean_up();
        let _ = std::fs::remove_dir_all(std::path::Path::new(&record).parent().unwrap());
    }

    /// From a host the key does not allow only B4 can run: everything else
    /// would be refused for the host, which says nothing about the cases.
    #[tokio::test]
    async fn from_a_host_the_key_does_not_allow_only_b4_runs() {
        let mut venue = MiniVenue::documented();
        venue.whitelisted = false;
        let lab = Lab::new("second-host", venue).await;
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        let mut flagged = settings();
        flagged.expect_not_whitelisted = true;
        let mut lv1 = Lv1Binance::new(&run, &env, lab.server.uri(), fast(), flagged);

        lv1.run_all().await.unwrap();

        let lines = lab.setup.lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            (lines[0]["case"].as_str(), lines[0]["verdict"].as_str()),
            (Some("B4"), Some("pass"))
        );
        assert!(run.finish().is_ok());
        lab.setup.clean_up();
    }

    /// A run stops at once when a request that must be refused is accepted: the
    /// cases after it are not run.
    #[tokio::test]
    async fn a_run_stops_at_the_first_request_the_venue_accepted() {
        let mut venue = MiniVenue::documented();
        venue.accepts_everything = true;
        let lab = Lab::new("stops", venue).await;
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        let mut lv1 = Lv1Binance::new(&run, &env, lab.server.uri(), fast(), settings());

        lv1.run_all().await.unwrap();

        let cases: Vec<String> = lab
            .setup
            .lines()
            .iter()
            .map(|line| line["case"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            cases,
            ["B10", "B10", "B1"],
            "B1 was accepted: nothing after it ran"
        );
        assert!(run.finish().is_err());
        lab.setup.clean_up();
    }

    /// "Every ignored test supports it": a dry run of the whole of LV1 prints
    /// the calls that need no read and sends none.
    #[tokio::test]
    async fn a_dry_run_of_the_whole_of_lv1_prints_its_calls_and_sends_none() {
        let lab = Lab::new("dry", MiniVenue::documented())
            .await
            .with("VP_DRY_RUN", "1");
        let env = lab.setup.env();
        let run = lab.setup.run(&lab.server).unwrap();
        let mut lv1 = Lv1Binance::new(&run, &env, lab.server.uri(), fast(), settings());

        lv1.run_all().await.unwrap();

        assert!(
            lab.received().await.is_empty(),
            "{:?}",
            lab.received().await
        );
        let printed = run.gate().printed();
        for call in [
            "DRY RUN GET /api/v3/exchangeInfo symbol=AEROUSDT",
            "DRY RUN GET /api/v3/account omitZeroBalances=true",
            "DRY RUN GET /api/v3/time",
            "DRY RUN GET /api/v3/ticker/bookTicker symbol=NOSUCHXXXUSDT",
            "DRY RUN GET /api/v3/account/commission symbol=AEROUSDT",
            "DRY RUN GET /sapi/v1/account/apiRestrictions",
            "DRY RUN GET /api/v3/depth symbol=AEROUSDT limit=5",
        ] {
            assert!(
                printed.iter().any(|line| line == call),
                "{call} not in {printed:#?}"
            );
        }
        assert!(
            !printed.iter().any(|line| line.contains("POST")),
            "an order is sized from a read that a dry run does not make: {printed:#?}"
        );
        let lines = lab.setup.lines();
        assert!(
            lines.iter().all(|line| line["verdict"] == "skipped"),
            "{lines:#?}"
        );
        assert!(run.finish().is_ok());
        lab.setup.clean_up();
    }

    #[test]
    fn the_settings_default_to_aerousdt_ten_dollars_and_five() {
        let env = crate::production::env::MapEnv::new(&[]);
        let settings = Settings::from_env(&env).unwrap();
        assert_eq!(settings.symbol, "AEROUSDT");
        assert_eq!(
            (settings.order_usd, settings.empty_max_usd),
            (d("10"), d("5"))
        );
        assert!(!settings.expect_not_whitelisted);

        let env = crate::production::env::MapEnv::new(&[
            ("VP_SYMBOL", "BTCUSDT"),
            ("VP_ORDER_USD", "7.5"),
            ("VP_EMPTY_MAX_USD", "2"),
            ("VP_EXPECT_NOT_WHITELISTED", "1"),
        ]);
        let settings = Settings::from_env(&env).unwrap();
        assert_eq!(settings.symbol, "BTCUSDT");
        assert_eq!(
            (settings.order_usd, settings.empty_max_usd),
            (d("7.5"), d("2"))
        );
        assert!(settings.expect_not_whitelisted);
    }

    // --- the test a person runs ---

    /// LV1 against Binance spot production:
    ///
    /// ```text
    /// VP_DRY_RUN=1 cargo test --lib production_lv1_binance -- --ignored --nocapture
    /// cargo test --lib production_lv1_binance -- --ignored --nocapture
    /// ```
    ///
    /// Needs `BINANCE_BASE_URL=https://api.binance.com`, `BINANCE_API_KEY` and
    /// `BINANCE_API_SECRET`, `VP_SPEND_CAP_USD`, `VP_HALT_FILE` and `VP_OUT`;
    /// optionally `BINANCE_RO_*`, `BINANCE_EMPTY_*`, `VP_SYMBOL`,
    /// `VP_ORDER_USD`, `VP_EMPTY_MAX_USD`, `VP_ORDER_CAP_USD`, `VP_RECORD_DIR`
    /// and, from a second host, `VP_EXPECT_NOT_WHITELISTED=1`.
    #[tokio::test]
    #[ignore = "runs against Binance production with a real key: see specs/V7-production-validation.md"]
    async fn production_lv1_binance() {
        let env = ProcessEnv;
        let run = Run::start(&env, "binance-spot", "api.binance.com", Echo::Stdout)
            .expect("the run's settings");
        let settings = Settings::from_env(&env).expect("the case settings");
        let mut lv1 = Lv1Binance::new(
            &run,
            &env,
            PRODUCTION_BINANCE_SPOT.to_string(),
            CexTimings::default(),
            settings,
        );
        lv1.run_all().await.expect("the run");
        run.finish().expect("LV1 is clean");
    }
}
