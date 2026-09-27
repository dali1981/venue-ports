/// Whether an outcome came from something that was actually sent, or from
/// something that was run and thrown away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// The call was run (an `eth_call`, a dry-run endpoint, a stub) and its
    /// result was never sent anywhere real.
    Simulated,
    /// A real transaction or order was actually sent to the venue.
    Landed,
}
