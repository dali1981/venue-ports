//! Test-only hooks on [`BinanceClient`] for the production tier
//! (`src/production`, `specs/V7-production-validation.md`).
//!
//! The harness table asks for three requests the normal path never makes: a
//! signed request with a timestamp chosen by the caller, one with its
//! signature left out, and one with its signature damaged by one byte. None
//! goes through [`BinanceClient::signed`], which reads the clock again and
//! retries once on `-1021`: that retry is why a stale timestamp could not
//! otherwise be observed. Each sends exactly one request.
//!
//! The same module is where the client reaches its [`Wire`]: asked before
//! each call, told each reply.

use super::*;
use crate::production::wire::{Call, Wire};

/// How a request's signature is damaged before it is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Damage {
    /// Not at all.
    None,
    /// The `signature` parameter is not sent.
    Omitted,
    /// One byte of the signature is changed.
    OneByte,
}

impl BinanceClient {
    /// This client, asking `wire` before each call and telling it each reply.
    pub(crate) fn with_wire(mut self, wire: Arc<dyn Wire>) -> Self {
        self.wire = Some(wire);
        self
    }

    /// Asks the wire whether `method path` may be made. An answer of no is a
    /// call that was never sent.
    pub(super) fn gate(
        &self,
        method: &Method,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<(), ApiError> {
        match &self.wire {
            Some(wire) => wire
                .permit(&Call::new(method, path, params))
                .map_err(|held| ApiError::NotSent(held.into_error())),
            None => Ok(()),
        }
    }

    /// Tells the wire a request is about to leave, signed and built.
    pub(super) fn sending(&self, method: &str, path: &str) {
        if let Some(wire) = &self.wire {
            wire.sending(method, path);
        }
    }

    /// Tells the wire what the venue answered, byte for byte.
    pub(super) fn observe(&self, method: &str, path: &str, status: u16, body: &str) {
        if let Some(wire) = &self.wire {
            wire.observed(method, path, status, body);
        }
    }

    /// A signed request whose timestamp is `timestamp_ms`, as given: the
    /// clock is not read and a `-1021` is not retried. With a timestamp
    /// outside `recvWindow` it observes the venue's `-1021`.
    pub(crate) async fn signed_with_timestamp<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
        timestamp_ms: u64,
    ) -> Result<T, ApiError> {
        self.damaged(method, path, params, Some(timestamp_ms), Damage::None)
            .await
    }

    /// A signed request sent without its `signature`, with a timestamp from
    /// the venue's clock. One request; no retry.
    pub(crate) async fn signed_without_signature<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T, ApiError> {
        self.damaged(method, path, params, None, Damage::Omitted)
            .await
    }

    /// A signed request whose signature has one byte changed, with a
    /// timestamp from the venue's clock. One request; no retry.
    pub(crate) async fn signed_with_corrupted_signature<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T, ApiError> {
        self.damaged(method, path, params, None, Damage::OneByte)
            .await
    }

    async fn damaged<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
        timestamp_ms: Option<u64>,
        damage: Damage,
    ) -> Result<T, ApiError> {
        self.gate(&method, path, params)?;
        let timestamp = match timestamp_ms {
            Some(timestamp) => timestamp,
            None => self.timestamp().await.map_err(ApiError::NotSent)?.0,
        };
        let query = sign::signed_query(
            &self.api_secret,
            params,
            self.timings.recv_window.as_millis() as u64,
            timestamp,
        );
        let query = damage_signature(&query, damage);
        self.send(method, path, &query, true, Instant::now()).await
    }
}

/// `query`, as `sign::signed_query` built it (the signature is its last
/// parameter), with its signature damaged as `damage` says.
fn damage_signature(query: &str, damage: Damage) -> String {
    let (unsigned, signature) = query
        .rsplit_once("&signature=")
        .expect("a signed query ends in its signature");
    match damage {
        Damage::None => query.to_string(),
        Damage::Omitted => unsigned.to_string(),
        Damage::OneByte => {
            let mut bytes = hex::decode(signature).expect("a signature is hex");
            let last = bytes.last_mut().expect("a signature is not empty");
            *last ^= 0xFF;
            format!("{unsigned}&signature={}", hex::encode(bytes))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &str = "secret";

    fn client(server: &MockServer) -> BinanceClient {
        BinanceClient::new(
            server.uri(),
            "key".to_string(),
            SECRET.to_string(),
            "/time",
            CexTimings {
                request_timeout: Duration::from_millis(200),
                recv_window: Duration::from_millis(1_000),
                ..CexTimings::default()
            },
        )
    }

    /// A venue that refuses every `/private` request the way Binance refuses a
    /// stale timestamp, and has a clock.
    async fn venue_refusing_the_timestamp() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/time"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "serverTime": local_now_ms() })),
            )
            .mount(&server)
            .await;
        Mock::given(path("/private"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": -1021, "msg": "Timestamp for this request is outside of the recvWindow."
            })))
            .mount(&server)
            .await;
        server
    }

    async fn private_requests(server: &MockServer) -> Vec<wiremock::Request> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/private")
            .collect()
    }

    fn query_of(request: &wiremock::Request) -> String {
        request.url.query().unwrap_or_default().to_string()
    }

    /// B3 cannot be observed through `signed`, which would read the clock
    /// again and retry. The hook sends the timestamp it was given, once.
    #[tokio::test]
    async fn an_explicit_timestamp_is_sent_as_given_without_reading_the_clock_or_retrying() {
        let server = venue_refusing_the_timestamp().await;

        let err = client(&server)
            .signed_with_timestamp::<serde_json::Value>(
                Method::GET,
                "/private",
                &[("symbol", "AEROUSDT".to_string())],
                1_000,
            )
            .await
            .unwrap_err();

        assert_eq!(err.code(), Some(TIMESTAMP_OUTSIDE_RECV_WINDOW));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "one request, no clock read, no retry");
        let query = query_of(&requests[0]);
        let unsigned = "symbol=AEROUSDT&recvWindow=1000&timestamp=1000";
        assert_eq!(
            query,
            format!("{unsigned}&signature={}", sign::sign(SECRET, unsigned)),
            "a correct signature over the timestamp it was given"
        );
        assert_eq!(requests[0].headers.get("X-MBX-APIKEY").unwrap(), "key");
    }

    /// The same request through the normal path is retried, which is why the
    /// hook exists.
    #[tokio::test]
    async fn the_normal_path_would_have_retried_it() {
        let server = venue_refusing_the_timestamp().await;
        let _ = client(&server)
            .signed::<serde_json::Value>(Method::GET, "/private", &[])
            .await;
        assert_eq!(private_requests(&server).await.len(), 2);
    }

    #[tokio::test]
    async fn a_signature_left_out_is_not_in_the_query_and_is_not_retried() {
        let server = venue_refusing_the_timestamp().await;

        let err = client(&server)
            .signed_without_signature::<serde_json::Value>(Method::GET, "/private", &[])
            .await
            .unwrap_err();

        assert_eq!(err.code(), Some(TIMESTAMP_OUTSIDE_RECV_WINDOW));
        let requests = private_requests(&server).await;
        assert_eq!(requests.len(), 1);
        let query = query_of(&requests[0]);
        assert!(!query.contains("signature"), "{query}");
        assert!(query.starts_with("recvWindow=1000&timestamp="), "{query}");
    }

    #[tokio::test]
    async fn a_corrupted_signature_differs_from_the_right_one_in_exactly_one_byte() {
        let server = venue_refusing_the_timestamp().await;

        let err = client(&server)
            .signed_with_corrupted_signature::<serde_json::Value>(
                Method::GET,
                "/private",
                &[("symbol", "AEROUSDT".to_string())],
            )
            .await
            .unwrap_err();

        assert_eq!(err.code(), Some(TIMESTAMP_OUTSIDE_RECV_WINDOW));
        let requests = private_requests(&server).await;
        assert_eq!(requests.len(), 1, "no retry");
        let query = query_of(&requests[0]);
        let (unsigned, sent) = query.rsplit_once("&signature=").unwrap();
        let right = sign::sign(SECRET, unsigned);
        assert_ne!(sent, right);
        assert_eq!(sent.len(), right.len());
        let differing = hex::decode(sent)
            .unwrap()
            .iter()
            .zip(hex::decode(&right).unwrap())
            .filter(|(a, b)| **a != *b)
            .count();
        assert_eq!(differing, 1, "{sent} against {right}");
    }
}
