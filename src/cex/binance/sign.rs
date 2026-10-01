//! HMAC-SHA256 query signing, shared by Binance spot and USDⓈ-M futures:
//! both APIs sign the same way.
//!
//! A signed call carries its parameters as a query string, then
//! `recvWindow` and `timestamp`, then a final `signature` parameter: the
//! hex HMAC-SHA256 of everything before it, keyed with the API secret. The
//! API key travels in the `X-MBX-APIKEY` header, never in the query string.
//! Binance verifies the signature over the query string exactly as it
//! arrives, so the string built here is the one sent, byte for byte. See
//! <https://developers.binance.com/docs/binance-spot-api-docs/rest-api/endpoint-security-type>.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// The hex HMAC-SHA256 of `payload` under `secret`.
pub(crate) fn sign(secret: &str, payload: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// `params` as a query string, in the order given, each value
/// percent-encoded. Symbols, decimals and the client order ids this crate
/// generates need no encoding; encoding anyway means an unexpected
/// character can never make the signed string differ from the sent one.
pub(crate) fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{key}={}", percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The exact query string of a signed call: `params` in the order given,
/// then `recvWindow`, then `timestamp`, then `signature` over everything
/// before it.
pub(crate) fn signed_query(
    secret: &str,
    params: &[(&str, String)],
    recv_window_ms: u64,
    timestamp_ms: u64,
) -> String {
    let mut unsigned = encode_query(params);
    if !unsigned.is_empty() {
        unsigned.push('&');
    }
    unsigned.push_str(&format!(
        "recvWindow={recv_window_ms}&timestamp={timestamp_ms}"
    ));
    let signature = sign(secret, &unsigned);
    format!("{unsigned}&signature={signature}")
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From memory of Binance's own API docs "Signed Endpoint Examples"
    /// (secret `"NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j"`,
    /// query string below) — this session has no network access to
    /// re-fetch and confirm it against the live docs, so treat a future
    /// mismatch here as reason to re-derive the vector from Binance's
    /// current docs, not as proof this signing code regressed.
    #[test]
    fn sign_matches_binances_own_documented_vector() {
        let secret = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
        let payload = "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&\
                        price=0.1&recvWindow=5000&timestamp=1499827319559";
        assert_eq!(
            sign(secret, payload),
            "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71"
        );
    }

    #[test]
    fn signed_query_reproduces_the_documented_vector_in_wire_order() {
        let secret = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
        let params = [
            ("symbol", "LTCBTC".to_string()),
            ("side", "BUY".to_string()),
            ("type", "LIMIT".to_string()),
            ("timeInForce", "GTC".to_string()),
            ("quantity", "1".to_string()),
            ("price", "0.1".to_string()),
        ];
        assert_eq!(
            signed_query(secret, &params, 5000, 1499827319559),
            "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1\
             &recvWindow=5000&timestamp=1499827319559\
             &signature=c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71"
        );
    }

    #[test]
    fn signed_query_without_params_still_signs_the_window_and_timestamp() {
        let query = signed_query("s", &[], 5000, 1);
        assert_eq!(
            query,
            format!(
                "recvWindow=5000&timestamp=1&signature={}",
                sign("s", "recvWindow=5000&timestamp=1")
            )
        );
    }

    #[test]
    fn encode_query_escapes_anything_outside_the_unreserved_set() {
        assert_eq!(
            encode_query(&[("a", "b c/d".to_string()), ("id", "vp-1_x.y~".to_string())]),
            "a=b%20c%2Fd&id=vp-1_x.y~"
        );
    }
}
