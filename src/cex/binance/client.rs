//! One HTTP client for any Binance REST API, spot or USDⓈ-M futures: the
//! same signing ([`super::sign`]), the same server clock
//! ([`super::clock`]), the same error body (`{"code": -2022, "msg": "…"}`),
//! and the same rule for what a failed call means.
//!
//! That rule is the point of this module. `SPEC.md` §6 says an `Err` from
//! `execute` means nothing filled, unless it is an `OrderStateUnknown`, so
//! every failure here is sorted by what it says about whether the venue
//! acted on the request ([`ApiError`]):
//!
//! - **Not sent**: the connection could not be opened, or the clock could
//!   not be read before signing. Nothing reached the venue.
//! - **Refused**: a 4xx answer. The venue read the request and turned it
//!   down (`-2022` reduce-only rejected, `-4164` notional, `-2019` margin,
//!   `-2013` no such order, a rate limit, …). It did not act on it. It
//!   reaches a caller as a [`VenueRefusal`], whose `code` is read by type.
//! - **Lost**: sent, but no readable answer came back — a timeout, a
//!   dropped connection, an HTTP 5xx (Binance documents these as "execution
//!   status unknown"), a 408, or a success whose body cannot be read. The
//!   venue may have acted on it.
//!
//! A `-1021` (timestamp outside `recvWindow`) means the request was not
//! processed, so a signed call answered with one reads the clock again and
//! is retried once.

use crate::cex::binance::clock::{local_now_ms, ServerClock};
use crate::cex::binance::sign;
use crate::cex::{CexTimings, VenueRefusal};
use anyhow::{anyhow, Context};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `-1021`: the timestamp is outside `recvWindow` against the venue's clock.
pub(crate) const TIMESTAMP_OUTSIDE_RECV_WINDOW: i64 = -1021;
/// `-2013`: the venue has no order matching the query.
pub(crate) const NO_SUCH_ORDER: i64 = -2013;

/// A failed call, sorted by whether the venue may have acted on it. See the
/// module docs.
#[derive(Debug)]
pub(crate) enum ApiError {
    /// Nothing reached the venue.
    NotSent(anyhow::Error),
    /// The venue answered with a refusal and did not act on the request.
    Refused(VenueRefusal),
    /// The request may have reached the venue, but no readable answer came
    /// back.
    Lost {
        /// When the request was signed (its `timestamp` was taken), so a
        /// caller can tell when `recvWindow` has passed for it.
        signed_at: Instant,
        cause: anyhow::Error,
    },
}

impl ApiError {
    /// The venue's error code, if it refused the request with one.
    pub(crate) fn code(&self) -> Option<i64> {
        match self {
            ApiError::Refused(refusal) => refusal.code,
            _ => None,
        }
    }

    /// This failure as an error whose text is `message`, which says what was
    /// being done and ends with this failure's own text. A refusal stays a
    /// [`VenueRefusal`] under it, so `to_string()` is `message` and a caller
    /// still reads the code by type; `{:#}` says the refusal's text once more.
    /// So does a refusal that is the cause of a request never sent (a refused
    /// clock read).
    pub(crate) fn because(self, message: String) -> anyhow::Error {
        match self {
            ApiError::Refused(refusal) => anyhow::Error::new(refusal).context(message),
            ApiError::NotSent(cause) => cause.context(message),
            ApiError::Lost { .. } => anyhow::Error::msg(message),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NotSent(cause) => write!(f, "not sent: {cause:#}"),
            ApiError::Refused(refusal) => write!(f, "{refusal}"),
            ApiError::Lost { cause, .. } => write!(f, "no readable answer: {cause:#}"),
        }
    }
}

/// How a failed call becomes the `anyhow::Error` a port returns: a refusal is
/// a [`VenueRefusal`], which a caller reads by type, and the other two say what
/// they always said.
///
/// `ApiError` is deliberately not a `std::error::Error`. Every `?`, `.into()`
/// and `anyhow::Error::from` on one goes through this, so a refusal cannot
/// reach a caller as anything but a `VenueRefusal`, and a new call that forgets
/// to convert does not compile.
impl From<ApiError> for anyhow::Error {
    fn from(err: ApiError) -> Self {
        match err {
            ApiError::Refused(refusal) => anyhow::Error::new(refusal),
            other => {
                let message = other.to_string();
                other.because(message)
            }
        }
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    code: i64,
    msg: String,
}

#[derive(Deserialize)]
struct ServerTime {
    #[serde(rename = "serverTime")]
    server_time: i64,
}

pub(crate) struct BinanceClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    api_secret: String,
    /// `/api/v3/time` (spot) or `/fapi/v1/time` (futures).
    time_path: &'static str,
    clock: Arc<ServerClock>,
    timings: CexTimings,
    /// The production harness's seam (`src/production`): asked before each
    /// call, told each reply. Test builds only; unset, the client is as it is.
    #[cfg(test)]
    wire: Option<Arc<dyn crate::production::wire::Wire>>,
}

impl BinanceClient {
    pub(crate) fn new(
        base_url: String,
        api_key: String,
        api_secret: String,
        time_path: &'static str,
        timings: CexTimings,
    ) -> Self {
        // No client-wide timeout: each request carries `request_timeout`
        // itself, so a copy with other timings can share this one's
        // connections. Redirects are refused, so a POST can never be
        // silently replayed as something else.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("building an HTTP client fails only as reqwest::Client::new() would panic");
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            api_secret,
            time_path,
            clock: Arc::new(ServerClock::new(timings.recv_window, timings.clock_refresh)),
            timings,
            #[cfg(test)]
            wire: None,
        }
    }

    /// The same client — connections, keys and clock — with other timings.
    /// For a test that must lose the answer to a request the venue has
    /// already acted on.
    #[cfg(test)]
    pub(crate) fn with_timings(&self, timings: CexTimings) -> Self {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            api_secret: self.api_secret.clone(),
            time_path: self.time_path,
            clock: Arc::clone(&self.clock),
            timings,
            wire: self.wire.clone(),
        }
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(crate) fn timings(&self) -> &CexTimings {
        &self.timings
    }

    /// The best estimate of the venue's time now, in Unix ms.
    pub(crate) fn venue_now_ms(&self) -> i64 {
        self.clock.estimate_now_ms()
    }

    /// An unsigned GET, for public endpoints.
    pub(crate) async fn public_get<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T, ApiError> {
        #[cfg(test)]
        self.gate(&Method::GET, path, params)?;
        let query = sign::encode_query(params);
        self.send(Method::GET, path, &query, false, Instant::now())
            .await
    }

    /// Reads the venue's clock now, refusing a round trip that does not fit
    /// inside `recvWindow`. Returns the round trip.
    pub(crate) async fn sync_clock(&self) -> anyhow::Result<Duration> {
        #[cfg(test)]
        self.gate(&Method::GET, self.time_path, &[])?;
        let sent_local_ms = local_now_ms();
        let started = Instant::now();
        let time: ServerTime = self
            .send(Method::GET, self.time_path, "", false, started)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("reading the venue's clock at {}", self.time_path))?;
        let rtt = started.elapsed();
        self.clock.record(time.server_time, sent_local_ms, rtt)?;
        Ok(rtt)
    }

    /// When `recvWindow` has certainly passed, by the venue's clock, for a
    /// request signed at `signed_at`: the window itself, plus the clock
    /// estimate's error bound. After that the venue can no longer accept
    /// the request, so "no such order" is conclusive.
    pub(crate) fn recv_window_ends(&self, signed_at: Instant) -> Instant {
        signed_at + self.timings.recv_window + self.clock.error_bound()
    }

    /// A signed call, with `params` in the query string in the order given.
    /// Answered with `-1021`, it reads the clock again and is retried once.
    pub(crate) async fn signed<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T, ApiError> {
        #[cfg(test)]
        self.gate(&method, path, params)?;
        let mut resynced = false;
        loop {
            let (timestamp, signed_at) = self.timestamp().await.map_err(ApiError::NotSent)?;
            let query = sign::signed_query(
                &self.api_secret,
                params,
                self.timings.recv_window.as_millis() as u64,
                timestamp,
            );
            match self
                .send(method.clone(), path, &query, true, signed_at)
                .await
            {
                Err(ApiError::Refused(VenueRefusal {
                    code: Some(TIMESTAMP_OUTSIDE_RECV_WINDOW),
                    ..
                })) if !resynced => {
                    self.clock.invalidate();
                    resynced = true;
                }
                result => return result,
            }
        }
    }

    /// A timestamp from the venue's clock, reading it first if the last
    /// reading is missing or stale, and the instant it was taken.
    async fn timestamp(&self) -> anyhow::Result<(u64, Instant)> {
        if self.clock.fresh_now_ms().is_none() {
            self.sync_clock().await?;
        }
        let signed_at = Instant::now();
        let now = self
            .clock
            .fresh_now_ms()
            .unwrap_or_else(|| self.clock.estimate_now_ms());
        let timestamp = u64::try_from(now)
            .map_err(|_| anyhow!("the venue's clock reads {now}, before the Unix epoch"))?;
        Ok((timestamp, signed_at))
    }

    async fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &str,
        signed: bool,
        signed_at: Instant,
    ) -> Result<T, ApiError> {
        let url = if query.is_empty() {
            format!("{}{path}", self.base_url)
        } else {
            format!("{}{path}?{query}", self.base_url)
        };
        let mut request = self
            .http
            .request(method.clone(), url)
            .timeout(self.timings.request_timeout);
        if signed {
            request = request.header("X-MBX-APIKEY", &self.api_key);
        }

        #[cfg(test)]
        self.sending(method.as_str(), path);
        let response = match request.send().await {
            Ok(response) => response,
            // Failing to open the connection, or to build the request at
            // all, happens before a byte of it is written.
            Err(err) if err.is_connect() || err.is_builder() => {
                return Err(ApiError::NotSent(
                    anyhow::Error::new(err).context(format!("{method} {path}")),
                ))
            }
            Err(err) => {
                return Err(ApiError::Lost {
                    signed_at,
                    cause: anyhow::Error::new(err).context(format!("{method} {path}")),
                })
            }
        };

        let status = response.status();
        let body = response.text().await;
        #[cfg(test)]
        if let Ok(text) = &body {
            self.observe(method.as_str(), path, status.as_u16(), text);
        }
        let lost = |cause: anyhow::Error| ApiError::Lost {
            signed_at,
            cause: cause.context(format!("{method} {path} answered HTTP {status}")),
        };

        if status.is_success() {
            let body = body.map_err(|err| lost(err.into()))?;
            return serde_json::from_str(&body)
                .map_err(|err| lost(anyhow::Error::new(err).context(format!("body: {body}"))));
        }
        if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT {
            let body = body.unwrap_or_default();
            return Err(lost(anyhow!(
                "the venue's execution status is unknown: {body}"
            )));
        }
        if status.is_client_error() {
            let body = body.unwrap_or_default();
            return Err(ApiError::Refused(
                match serde_json::from_str::<ErrorBody>(&body) {
                    Ok(error) => VenueRefusal {
                        status: status.as_u16(),
                        code: Some(error.code),
                        msg: error.msg,
                    },
                    Err(_) => VenueRefusal {
                        status: status.as_u16(),
                        code: None,
                        msg: body,
                    },
                },
            ));
        }
        // 1xx or 3xx: nothing this client asked for, and nothing that says
        // the venue did not act.
        Err(lost(anyhow!("unexpected HTTP status")))
    }
}

#[cfg(test)]
mod hooks;

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn timings() -> CexTimings {
        CexTimings {
            request_timeout: Duration::from_millis(200),
            recv_window: Duration::from_millis(1_000),
            ..CexTimings::default()
        }
    }

    fn client(server: &MockServer) -> BinanceClient {
        BinanceClient::new(
            server.uri(),
            "key".to_string(),
            "secret".to_string(),
            "/time",
            timings(),
        )
    }

    async fn mount_time(server: &MockServer, server_time: i64) {
        Mock::given(method("GET"))
            .and(path("/time"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "serverTime": server_time })),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn signs_with_the_venues_clock_not_the_local_one() {
        let server = MockServer::start().await;
        // The venue's clock is an hour behind this one.
        let venue_now = local_now_ms() - 3_600_000;
        mount_time(&server, venue_now).await;
        Mock::given(method("GET"))
            .and(path("/private"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let _: serde_json::Value = client(&server)
            .signed(Method::GET, "/private", &[])
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let private = requests
            .iter()
            .find(|r| r.url.path() == "/private")
            .unwrap();
        let timestamp: i64 = private
            .url
            .query_pairs()
            .find(|(k, _)| k == "timestamp")
            .unwrap()
            .1
            .parse()
            .unwrap();
        assert!(
            (timestamp - venue_now).abs() < 1_000,
            "signed {timestamp}, venue clock {venue_now}"
        );
        assert_eq!(private.headers.get("X-MBX-APIKEY").unwrap(), "key");
    }

    #[tokio::test]
    async fn a_minus_1021_reads_the_clock_again_and_retries_once() {
        let server = MockServer::start().await;
        mount_time(&server, local_now_ms()).await;
        Mock::given(method("GET"))
            .and(path("/private"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -1021, "msg": "Timestamp for this request is outside of the recvWindow."
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/private"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": 1})))
            .mount(&server)
            .await;

        let answer: serde_json::Value = client(&server)
            .signed(Method::GET, "/private", &[])
            .await
            .unwrap();

        assert_eq!(answer["ok"], 1);
        let requests = server.received_requests().await.unwrap();
        let times = requests.iter().filter(|r| r.url.path() == "/time").count();
        assert_eq!(times, 2, "the clock is read at first use and after -1021");
    }

    #[tokio::test]
    async fn a_second_minus_1021_is_a_refusal() {
        let server = MockServer::start().await;
        mount_time(&server, local_now_ms()).await;
        Mock::given(method("GET"))
            .and(path("/private"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -1021, "msg": "Timestamp for this request is outside of the recvWindow."
            })))
            .expect(2)
            .mount(&server)
            .await;

        let err = client(&server)
            .signed::<serde_json::Value>(Method::GET, "/private", &[])
            .await
            .unwrap_err();

        assert_eq!(err.code(), Some(TIMESTAMP_OUTSIDE_RECV_WINDOW));
    }

    #[tokio::test]
    async fn failures_are_sorted_by_whether_the_venue_may_have_acted() {
        let server = MockServer::start().await;
        mount_time(&server, local_now_ms()).await;
        for (route, response) in [
            (
                "/5xx",
                ResponseTemplate::new(503).set_body_string("Unknown error"),
            ),
            ("/408", ResponseTemplate::new(408)),
            (
                "/garbled",
                ResponseTemplate::new(200).set_body_string("{not json"),
            ),
            (
                "/slow",
                ResponseTemplate::new(200).set_delay(Duration::from_millis(600)),
            ),
            (
                "/refused",
                ResponseTemplate::new(400).set_body_json(
                    serde_json::json!({"code": -2022, "msg": "ReduceOnly Order is rejected."}),
                ),
            ),
            (
                "/waf",
                ResponseTemplate::new(403).set_body_string("forbidden"),
            ),
        ] {
            Mock::given(path(route))
                .respond_with(response)
                .mount(&server)
                .await;
        }
        let client = client(&server);

        for route in ["/5xx", "/408", "/garbled", "/slow"] {
            let err = client
                .signed::<serde_json::Value>(Method::POST, route, &[])
                .await
                .unwrap_err();
            assert!(matches!(err, ApiError::Lost { .. }), "{route}: {err}");
        }
        let err = client
            .signed::<serde_json::Value>(Method::POST, "/refused", &[])
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ApiError::Refused(VenueRefusal { status: 400, .. })
        ));
        assert_eq!(err.code(), Some(-2022));
        assert!(err.to_string().contains("ReduceOnly Order is rejected."));

        let err = client
            .signed::<serde_json::Value>(Method::POST, "/waf", &[])
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ApiError::Refused(VenueRefusal {
                status: 403,
                code: None,
                ..
            })
        ));
    }

    /// A refusal reaches a caller as a `VenueRefusal`, saying what a refused
    /// call always said. The other two failures say what they always said and
    /// are not refusals: the venue may have acted on the first, and the second
    /// never heard of the request.
    #[tokio::test]
    async fn a_refusal_becomes_a_venue_refusal_and_the_other_failures_do_not() {
        use crate::cex::refusal_of;

        let server = MockServer::start().await;
        mount_time(&server, local_now_ms()).await;
        for (route, response) in [
            (
                "/refused",
                ResponseTemplate::new(400).set_body_json(
                    serde_json::json!({"code": -2022, "msg": "ReduceOnly Order is rejected."}),
                ),
            ),
            (
                "/5xx",
                ResponseTemplate::new(503).set_body_string("Unknown error"),
            ),
        ] {
            Mock::given(path(route))
                .respond_with(response)
                .mount(&server)
                .await;
        }
        let client = client(&server);

        let refused: anyhow::Error = client
            .signed::<serde_json::Value>(Method::POST, "/refused", &[])
            .await
            .unwrap_err()
            .into();
        assert_eq!(
            refusal_of(&refused),
            Some(&VenueRefusal {
                status: 400,
                code: Some(-2022),
                msg: "ReduceOnly Order is rejected.".to_string(),
            })
        );
        assert_eq!(
            refused.to_string(),
            "refused (HTTP 400, code -2022): ReduceOnly Order is rejected."
        );

        let lost: anyhow::Error = client
            .signed::<serde_json::Value>(Method::POST, "/5xx", &[])
            .await
            .unwrap_err()
            .into();
        assert!(refusal_of(&lost).is_none(), "{lost:#}");
        assert!(
            lost.to_string().starts_with("no readable answer: "),
            "{lost}"
        );

        // Nothing listens on port 9 (discard) on a test machine.
        let unsent: anyhow::Error = BinanceClient::new(
            "http://127.0.0.1:9".to_string(),
            "key".to_string(),
            "secret".to_string(),
            "/time",
            timings(),
        )
        .signed::<serde_json::Value>(Method::POST, "/order", &[])
        .await
        .unwrap_err()
        .into();
        assert!(refusal_of(&unsent).is_none(), "{unsent:#}");
        assert!(unsent.to_string().starts_with("not sent: "), "{unsent}");
    }

    /// A clock read the venue refuses (a rate limit on `/time`, a ban) is a
    /// refusal, on its own and as the reason a signed request was never sent,
    /// whose text is what it was.
    #[tokio::test]
    async fn a_refused_clock_read_is_a_venue_refusal_even_when_it_stops_a_request() {
        use crate::cex::refusal_of;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/time"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"code": -1003, "msg": "Too many requests."})),
            )
            .mount(&server)
            .await;
        let client = client(&server);

        let clock = client.sync_clock().await.unwrap_err();
        assert_eq!(clock.to_string(), "reading the venue's clock at /time");
        assert_eq!(refusal_of(&clock).unwrap().code, Some(-1003));

        let unsent: anyhow::Error = client
            .signed::<serde_json::Value>(Method::POST, "/order", &[])
            .await
            .unwrap_err()
            .into();
        assert_eq!(
            unsent.to_string(),
            "not sent: reading the venue's clock at /time: refused (HTTP 400, code -1003): \
             Too many requests."
        );
        assert_eq!(refusal_of(&unsent).unwrap().code, Some(-1003));
    }

    #[tokio::test]
    async fn a_connection_that_cannot_be_opened_was_never_sent() {
        // Nothing listens on port 9 (discard) on a test machine.
        let client = BinanceClient::new(
            "http://127.0.0.1:9".to_string(),
            "key".to_string(),
            "secret".to_string(),
            "/time",
            timings(),
        );
        // The clock is read first, and that fails to connect.
        let err = client
            .signed::<serde_json::Value>(Method::POST, "/order", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::NotSent(_)), "{err}");
    }
}
