//! Reading a Binance spot account's balances (`SPEC.md` §6c): `GET
//! /api/v3/account?omitZeroBalances=true` through the signed client, so the
//! venue's clock and the error rule are the order path's. See
//! <https://developers.binance.com/docs/binance-spot-api-docs/rest-api/account-endpoints#account-information-user_data>.
//!
//! The balances are the venue's own numbers, exact decimals, with `free` and
//! `locked` apart: `locked` is what an open order holds, still the account's.
//! With zero balances omitted, an asset that is not listed is held at zero.
//! The account's `uid`, permissions and commission rates are not read.

use crate::balance::{SpotAccountBalances, SpotBalance, SpotBalanceReader};
use crate::cex::binance::{BinanceLive, BinanceRest};
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Method;
use rust_decimal::Decimal;
use serde::Deserialize;

const ACCOUNT_PATH: &str = "/api/v3/account";

/// The subset of `GET /api/v3/account` this crate reads.
#[derive(Debug, Deserialize)]
struct AccountResponse {
    balances: Vec<BalanceLine>,
    /// When the account last changed, by the venue's clock.
    #[serde(rename = "updateTime", default)]
    update_time: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct BalanceLine {
    asset: String,
    free: Decimal,
    locked: Decimal,
}

impl From<AccountResponse> for SpotAccountBalances {
    fn from(account: AccountResponse) -> Self {
        Self {
            balances: account
                .balances
                .into_iter()
                .map(|line| SpotBalance {
                    asset: line.asset,
                    free: line.free,
                    locked: line.locked,
                })
                .collect(),
            update_time_ms: account.update_time,
        }
    }
}

#[async_trait]
impl SpotBalanceReader for BinanceRest {
    async fn balances(&self) -> Result<SpotAccountBalances> {
        let params = [("omitZeroBalances", "true".to_string())];
        let account: AccountResponse = self
            .client()
            .signed(Method::GET, ACCOUNT_PATH, &params)
            .await
            .map_err(anyhow::Error::new)
            .with_context(|| format!("reading GET {ACCOUNT_PATH}"))?;
        Ok(account.into())
    }

    fn label(&self) -> &'static str {
        "binance-spot-balances"
    }
}

/// The account `BinanceLive` trades on, read through its own client.
#[async_trait]
impl SpotBalanceReader for BinanceLive {
    async fn balances(&self) -> Result<SpotAccountBalances> {
        self.rest().balances().await
    }

    fn label(&self) -> &'static str {
        "binance-spot-balances"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cex::binance::clock::local_now_ms;
    use crate::cex::binance::BinanceConfig;
    use crate::cex::CexTimings;
    use crate::testkit::contract::spot_balance_reader_contract;
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // TODO(R2): replace with the recorded body. Copied verbatim from the Spot API
    // documentation, "Account information (USER_DATA)", Response:
    // https://github.com/binance/binance-spot-api-docs/blob/master/rest-api.md#account-information-user_data
    // The documentation's body has no `//` comments; nothing in it is edited.
    const DOCUMENTED_ACCOUNT: &str = r#"{
    "makerCommission": 15,
    "takerCommission": 15,
    "buyerCommission": 0,
    "sellerCommission": 0,
    "commissionRates": {
        "maker": "0.00150000",
        "taker": "0.00150000",
        "buyer": "0.00000000",
        "seller": "0.00000000"
    },
    "canTrade": true,
    "canWithdraw": true,
    "canDeposit": true,
    "brokered": false,
    "requireSelfTradePrevention": false,
    "preventSor": false,
    "updateTime": 123456789,
    "accountType": "SPOT",
    "balances": [
        {
            "asset": "BTC",
            "free": "4723846.89208129",
            "locked": "0.00000000"
        },
        {
            "asset": "LTC",
            "free": "4763368.68006011",
            "locked": "0.00000000"
        }
    ],
    "permissions": ["SPOT"],
    "uid": 354937868
}"#;

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

    async fn mount_account(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(ACCOUNT_PATH))
            .respond_with(response)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn the_documented_account_reads_as_exact_free_and_locked_balances() -> Result<()> {
        let server = venue().await;
        mount_account(
            &server,
            ResponseTemplate::new(200).set_body_raw(DOCUMENTED_ACCOUNT, "application/json"),
        )
        .await;

        let account = rest(&server).balances().await?;

        assert_eq!(account.update_time_ms, Some(123_456_789));
        assert_eq!(
            account.balances,
            vec![
                SpotBalance {
                    asset: "BTC".to_string(),
                    free: decimal("4723846.89208129"),
                    locked: Decimal::ZERO,
                },
                SpotBalance {
                    asset: "LTC".to_string(),
                    free: decimal("4763368.68006011"),
                    locked: Decimal::ZERO,
                },
            ]
        );
        // Exact: the eight decimals the venue printed, no float in between.
        assert_eq!(
            account.balance("BTC").map(|b| b.free.to_string()),
            Some("4723846.89208129".to_string())
        );
        assert_eq!(account.balance("ETH"), None);
        Ok(())
    }

    /// The call is signed with the venue's clock, carries the key, and asks for
    /// the non-zero balances only.
    #[tokio::test]
    async fn the_read_is_a_signed_get_that_omits_zero_balances() -> Result<()> {
        let server = venue().await;
        mount_account(
            &server,
            ResponseTemplate::new(200).set_body_raw(DOCUMENTED_ACCOUNT, "application/json"),
        )
        .await;

        rest(&server).balances().await?;

        let requests = server.received_requests().await.unwrap();
        let read = requests
            .iter()
            .find(|r| r.url.path() == ACCOUNT_PATH)
            .expect("the account was read");
        let param = |key: &str| {
            read.url
                .query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        };
        assert_eq!(read.method.as_str(), "GET");
        assert_eq!(param("omitZeroBalances").as_deref(), Some("true"));
        assert!(param("timestamp").is_some() && param("signature").is_some());
        assert_eq!(read.headers.get("X-MBX-APIKEY").unwrap(), "test-key");
        Ok(())
    }

    /// `BinanceLive` reads the account it trades on, through the same client
    /// and the same contract suite.
    #[tokio::test]
    async fn binance_live_reads_its_own_account_and_passes_the_contract() {
        let server = venue().await;
        mount_account(
            &server,
            ResponseTemplate::new(200).set_body_raw(DOCUMENTED_ACCOUNT, "application/json"),
        )
        .await;
        let live = BinanceLive::new(
            rest(&server),
            HashMap::from([("AEROUSDT".to_string(), decimal("0.1"))]),
        );

        spot_balance_reader_contract(&live).await;
        spot_balance_reader_contract(&rest(&server)).await;
    }

    /// A refusal is an `Err` carrying the venue's code, never an empty account.
    // TODO(R2): the body is the documented example of an error payload
    // (errors.md, "Error codes for Binance"); only its shape matters here.
    #[tokio::test]
    async fn a_refused_read_is_an_error_carrying_the_venues_code() {
        let server = venue().await;
        mount_account(
            &server,
            ResponseTemplate::new(400).set_body_raw(
                r#"{
    "code": -1121,
    "msg": "Invalid symbol."
}"#,
                "application/json",
            ),
        )
        .await;

        let err = rest(&server).balances().await.unwrap_err();

        let message = format!("{err:#}");
        assert!(message.contains("-1121"), "{message}");
        assert!(message.contains(ACCOUNT_PATH), "{message}");
    }

    #[tokio::test]
    async fn a_5xx_is_an_error_not_an_empty_account() {
        let server = venue().await;
        mount_account(
            &server,
            ResponseTemplate::new(503).set_body_string("Unknown error"),
        )
        .await;

        let err = rest(&server).balances().await.unwrap_err();

        assert!(
            format!("{err:#}").contains("execution status is unknown"),
            "{err:#}"
        );
    }
}
