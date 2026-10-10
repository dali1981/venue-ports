//! The quantity arithmetic the order cases share, in `Decimal` with every
//! operation checked: a quantity that cannot be computed is an error, never a
//! zero.

use anyhow::{anyhow, bail, Result};
use rust_decimal::Decimal;

/// `quantity` rounded down to a whole number of `step`s.
pub(crate) fn round_down(quantity: Decimal, step: Decimal) -> Result<Decimal> {
    if step <= Decimal::ZERO {
        bail!("a lot step of {step} cannot be rounded to");
    }
    let steps = quantity
        .checked_div(step)
        .ok_or_else(|| anyhow!("{quantity} over a step of {step} cannot be computed"))?
        .floor();
    steps
        .checked_mul(step)
        .ok_or_else(|| anyhow!("{steps} steps of {step} overflows"))
}

/// The quantity of `usd` at `price`, rounded down to a whole number of `step`s.
pub(crate) fn quantity_for(usd: Decimal, price: Decimal, step: Decimal) -> Result<Decimal> {
    if price <= Decimal::ZERO {
        bail!("a price of {price} cannot size an order");
    }
    round_down(
        usd.checked_div(price)
            .ok_or_else(|| anyhow!("{usd} over a price of {price} cannot be computed"))?,
        step,
    )
}

/// Whether `quantity` is a whole number of `step`s.
pub(crate) fn is_on_step(quantity: Decimal, step: Decimal) -> bool {
    step > Decimal::ZERO
        && quantity
            .checked_div(step)
            .is_some_and(|steps| steps == steps.floor())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(text: &str) -> Decimal {
        Decimal::from_str(text).unwrap()
    }

    #[test]
    fn a_quantity_is_rounded_down_to_the_step_and_never_up() {
        assert_eq!(round_down(d("10.037"), d("0.01")).unwrap(), d("10.03"));
        assert_eq!(round_down(d("10.03"), d("0.01")).unwrap(), d("10.03"));
        assert_eq!(round_down(d("0.009"), d("0.01")).unwrap(), d("0"));
        assert_eq!(round_down(d("7"), d("5")).unwrap(), d("5"));
    }

    #[test]
    fn a_quantity_for_dollars_is_what_the_dollars_buy_whole_steps_of() {
        // 10 USD at 1.0010 is 9.99 AERO; a step of 0.1 gives 9.9.
        assert_eq!(
            quantity_for(d("10"), d("1.0010"), d("0.1")).unwrap(),
            d("9.9")
        );
        assert_eq!(quantity_for(d("4"), d("0.5"), d("1")).unwrap(), d("8"));
    }

    #[test]
    fn a_step_or_price_that_is_not_positive_is_an_error_not_a_quantity() {
        assert!(round_down(d("1"), d("0")).is_err());
        assert!(round_down(d("1"), d("-0.1")).is_err());
        assert!(quantity_for(d("10"), d("0"), d("0.1")).is_err());
        assert!(quantity_for(d("10"), d("-1"), d("0.1")).is_err());
    }

    #[test]
    fn a_figure_too_large_to_hold_is_an_error() {
        assert!(round_down(Decimal::MAX, d("0.0000001")).is_err());
    }

    #[test]
    fn on_the_step_means_a_whole_number_of_steps() {
        assert!(is_on_step(d("9.9"), d("0.1")));
        assert!(!is_on_step(d("9.95"), d("0.1")));
        assert!(is_on_step(d("0"), d("0.1")));
        assert!(!is_on_step(d("1"), d("0")));
    }
}
