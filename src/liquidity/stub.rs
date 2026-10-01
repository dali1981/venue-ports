//! `LiquidityStub` — an in-process fake `LiquidityExecutor` with no network
//! calls (`SPEC.md` §5b). Each `execute()` returns the next programmed
//! outcome: success with a given event, `Reverted { reason }`, `TimedOut`,
//! an `Err`, or `LandedUnread`. It records every `prepare` and `execute` it
//! receives, and its capabilities are the test's to set.
//!
//! **A command with no programmed outcome is an `Err` naming the command.**
//! Unlike `DexStub` and `CexStub`, there is no default: a fallback would
//! have to invent liquidity and amounts, and invented numbers in a test are
//! how a fake drifts away from the real thing.
//!
//! **It keeps the rules every venue keeps.** `prepare` makes the same
//! chain-free checks as every venue ([`check_command`]), and refuses what a
//! position cannot take — a `Remove` above its liquidity, a `Close` while it
//! holds liquidity or owes tokens, any command on a position it does not
//! hold — from the state its own events imply: `Opened` and `Added` add
//! liquidity, `Removed` takes it out and leaves owed whatever it released
//! and did not transfer, `Collected` clears what is owed, `Closed` forgets
//! the position. A programmed event that is not the command's is an error.

use crate::dex::{Outcome, Prepared, TxCost};
use crate::liquidity::{
    check_command, LandedUnread, LiquidityCapabilities, LiquidityCommand, LiquidityEvent,
    LiquidityExecutor, LiquidityReport, LiquidityRequest, PositionId, TokenPair,
};
use crate::Provenance;
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// One call exactly as the stub received it.
#[derive(Debug, Clone)]
pub enum LiquidityCall {
    Prepare {
        command: LiquidityCommand,
        request: LiquidityRequest,
        /// `None` when `prepare` refused the command.
        prepared: Option<Prepared>,
    },
    Execute {
        prepared: Prepared,
        /// The command `prepared` came from, when this stub prepared it.
        command: Option<LiquidityCommand>,
    },
}

enum Programmed {
    /// A whole report, its cost included.
    Exact(Result<LiquidityReport>),
    /// An outcome whose cost is filled in at `execute`, from the family of
    /// the `Prepared` it runs.
    Shaped {
        outcome: Outcome,
        event: Option<LiquidityEvent>,
        at: u64,
    },
}

/// What a held position holds, as its events say.
#[derive(Debug, Clone, Copy, Default)]
struct Held {
    liquidity: u128,
    owed: TokenPair,
}

pub struct LiquidityStub {
    capabilities: LiquidityCapabilities,
    programmed: Mutex<VecDeque<Programmed>>,
    calls: Mutex<Vec<LiquidityCall>>,
    prepared: Mutex<Vec<(Prepared, LiquidityCommand)>>,
    held: Mutex<HashMap<PositionId, Held>>,
}

impl LiquidityStub {
    pub fn new(capabilities: LiquidityCapabilities) -> Self {
        Self {
            capabilities,
            programmed: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            prepared: Mutex::new(Vec::new()),
            held: Mutex::new(HashMap::new()),
        }
    }

    /// Program the exact report the next `execute()` call returns, its cost
    /// included. Consumed in the order programmed.
    pub fn program_execute(&self, result: Result<LiquidityReport>) {
        self.programmed
            .lock()
            .unwrap()
            .push_back(Programmed::Exact(result));
    }

    fn program_shaped(&self, outcome: Outcome, event: Option<LiquidityEvent>, at: u64) {
        self.programmed
            .lock()
            .unwrap()
            .push_back(Programmed::Shaped { outcome, event, at });
    }

    /// Program a success with the event it reports.
    pub fn program_event(&self, event: LiquidityEvent, at: u64) {
        self.program_shaped(Outcome::Success, Some(event), at);
    }

    /// Program a revert with a specific reason, e.g. `"Price slippage
    /// check"`.
    pub fn program_reverted(&self, reason: impl Into<String>, at: u64) {
        self.program_shaped(
            Outcome::Reverted {
                reason: reason.into(),
            },
            None,
            at,
        );
    }

    /// Program a forced timeout: the transaction's fate is unknown.
    pub fn program_timed_out(&self, at: u64) {
        self.program_shaped(Outcome::TimedOut, None, at);
    }

    /// Program a plain error: nothing was sent for the command.
    pub fn program_error(&self, reason: impl std::fmt::Display) {
        self.program_execute(Err(anyhow!("{reason}")));
    }

    /// Program a [`LandedUnread`]: the command landed, but its outcome could
    /// not be read.
    pub fn program_landed_unread(&self, tx_ref: Vec<u8>, reason: impl Into<String>) {
        self.program_execute(Err(LandedUnread {
            tx_ref,
            reason: reason.into(),
        }
        .into()));
    }

    /// Hold a position opened elsewhere, with `liquidity` in it and `owed`
    /// owed, so a test can start a life part-way.
    pub fn hold(&self, position: PositionId, liquidity: u128, owed: TokenPair) {
        self.held
            .lock()
            .unwrap()
            .insert(position, Held { liquidity, owed });
    }

    /// Every call the stub has received so far, in the order received.
    pub fn calls(&self) -> Vec<LiquidityCall> {
        self.calls.lock().unwrap().clone()
    }

    /// What a position can take, from what the stub holds.
    fn check_position(&self, cmd: &LiquidityCommand) -> Result<()> {
        let Some(position) = cmd.position() else {
            return Ok(());
        };
        let held = self.held.lock().unwrap();
        let Some(held) = held.get(position) else {
            bail!(
                "the owner does not hold position 0x{}",
                hex::encode(&position.bytes)
            );
        };
        match cmd {
            LiquidityCommand::Remove { liquidity, .. } if *liquidity > held.liquidity => bail!(
                "a Remove of {liquidity} is above the position's liquidity, {}",
                held.liquidity
            ),
            LiquidityCommand::Close { .. } if held.liquidity > 0 || !held.owed.is_zero() => bail!(
                "a Close on a position that holds liquidity ({}) or owes tokens ({:?})",
                held.liquidity,
                held.owed
            ),
            _ => Ok(()),
        }
    }

    /// The state an event implies, after checking it is the command's.
    fn apply(&self, cmd: &LiquidityCommand, event: &LiquidityEvent) -> Result<()> {
        let mut held = self.held.lock().unwrap();
        match (cmd, event) {
            (
                LiquidityCommand::Open { .. },
                LiquidityEvent::Opened {
                    position,
                    liquidity,
                    ..
                },
            ) => {
                held.insert(
                    position.clone(),
                    Held {
                        liquidity: *liquidity,
                        owed: TokenPair::default(),
                    },
                );
            }
            (LiquidityCommand::Add { position, .. }, LiquidityEvent::Added { liquidity, .. }) => {
                let h = held.entry(position.clone()).or_default();
                h.liquidity = h.liquidity.saturating_add(*liquidity);
            }
            (
                LiquidityCommand::Remove { position, .. },
                LiquidityEvent::Removed {
                    liquidity,
                    released,
                    transferred,
                },
            ) => {
                let h = held.entry(position.clone()).or_default();
                h.liquidity = h.liquidity.saturating_sub(*liquidity);
                h.owed = TokenPair::new(
                    h.owed.token0 + released.token0.saturating_sub(transferred.token0),
                    h.owed.token1 + released.token1.saturating_sub(transferred.token1),
                );
            }
            (LiquidityCommand::Collect { position }, LiquidityEvent::Collected { .. }) => {
                held.entry(position.clone()).or_default().owed = TokenPair::default();
            }
            (LiquidityCommand::Close { position }, LiquidityEvent::Closed) => {
                held.remove(position);
            }
            (cmd, event) => bail!(
                "LiquidityStub was programmed with {} for a {} command",
                event.kind(),
                cmd.kind()
            ),
        }
        Ok(())
    }
}

#[async_trait]
impl LiquidityExecutor for LiquidityStub {
    fn capabilities(&self) -> LiquidityCapabilities {
        self.capabilities
    }

    async fn prepare(&self, cmd: &LiquidityCommand, req: &LiquidityRequest) -> Result<Prepared> {
        let result = check_command(cmd, req, &self.capabilities)
            .and_then(|()| self.check_position(cmd))
            .map(|()| {
                let mut prepared = self.prepared.lock().unwrap();
                let value = Prepared::offline(cmd.network(), &req.owner, prepared.len() as u64);
                prepared.push((value.clone(), cmd.clone()));
                value
            });
        self.calls.lock().unwrap().push(LiquidityCall::Prepare {
            command: cmd.clone(),
            request: req.clone(),
            prepared: result.as_ref().ok().cloned(),
        });
        result
    }

    async fn execute(&self, prepared: &Prepared) -> Result<LiquidityReport> {
        let command = self
            .prepared
            .lock()
            .unwrap()
            .iter()
            .find(|(value, _)| value == prepared)
            .map(|(_, cmd)| cmd.clone());
        self.calls.lock().unwrap().push(LiquidityCall::Execute {
            prepared: prepared.clone(),
            command: command.clone(),
        });
        let Some(command) = command else {
            bail!(
                "LiquidityStub has no programmed outcome for a Prepared value it did not prepare"
            );
        };
        let report = match self.programmed.lock().unwrap().pop_front() {
            Some(Programmed::Exact(result)) => result?,
            Some(Programmed::Shaped { outcome, event, at }) => LiquidityReport {
                outcome,
                event,
                cost: TxCost::none_for(prepared),
                at,
                provenance: Provenance::Simulated,
                tx_ref: None,
            },
            None => bail!(
                "LiquidityStub has no programmed outcome for {command:?} — program one; the stub \
                 never invents liquidity or amounts"
            ),
        };
        if let Some(event) = &report.event {
            self.apply(&command, event)?;
        }
        Ok(report)
    }

    fn label(&self) -> &'static str {
        "liquidity-stub"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::liquidity::{unix_now, Deposit, DepositGuard, Range};
    use crate::Network;

    fn position() -> PositionId {
        PositionId {
            network: Network::evm(1),
            bytes: vec![7; 32],
        }
    }

    fn open() -> LiquidityCommand {
        LiquidityCommand::Open {
            range: Range {
                network: Network::evm(1),
                pool: vec![0x22; 20],
                tick_lower: -60,
                tick_upper: 60,
            },
            deposit: Deposit {
                max: TokenPair::new(100, 100),
                guard: DepositGuard::MinAmounts(TokenPair::default()),
            },
        }
    }

    fn request() -> LiquidityRequest {
        LiquidityRequest {
            owner: vec![0xCC; 20],
            deadline_unix_secs: unix_now() + 600,
        }
    }

    fn stub() -> LiquidityStub {
        LiquidityStub::new(LiquidityCapabilities::UNISWAP_V3)
    }

    async fn run(stub: &LiquidityStub, cmd: &LiquidityCommand) -> LiquidityReport {
        let prepared = stub.prepare(cmd, &request()).await.unwrap();
        stub.execute(&prepared).await.unwrap()
    }

    #[tokio::test]
    async fn a_command_with_no_programmed_outcome_is_an_error_naming_it() {
        let stub = stub();
        let prepared = stub.prepare(&open(), &request()).await.unwrap();

        let err = stub.execute(&prepared).await.unwrap_err();

        assert!(err.to_string().contains("Open"));
        assert!(err.to_string().contains("no programmed outcome"));
    }

    #[tokio::test]
    async fn returns_each_programmed_outcome_in_order() {
        let stub = stub();
        stub.program_event(
            LiquidityEvent::Opened {
                position: position(),
                liquidity: 1_000,
                paid: TokenPair::new(40, 60),
            },
            7,
        );
        stub.program_reverted("Price slippage check", 8);
        stub.program_timed_out(9);
        stub.program_landed_unread(vec![0xAB; 32], "no IncreaseLiquidity event");
        stub.program_error("approve reverted");

        let prepared = stub.prepare(&open(), &request()).await.unwrap();
        let success = stub.execute(&prepared).await.unwrap();
        assert!(matches!(success.outcome, Outcome::Success));
        assert_eq!(success.at, 7);
        assert!(matches!(
            success.event,
            Some(LiquidityEvent::Opened {
                liquidity: 1_000,
                ..
            })
        ));

        let reverted = stub.execute(&prepared).await.unwrap();
        match reverted.outcome {
            Outcome::Reverted { reason } => assert_eq!(reason, "Price slippage check"),
            other => panic!("expected Reverted, got {other:?}"),
        }
        assert_eq!(reverted.event, None);

        let timed_out = stub.execute(&prepared).await.unwrap();
        assert!(matches!(timed_out.outcome, Outcome::TimedOut));
        assert_eq!(timed_out.event, None);

        let unread = stub.execute(&prepared).await.unwrap_err();
        assert_eq!(
            unread
                .downcast_ref::<LandedUnread>()
                .map(|u| u.tx_ref.clone()),
            Some(vec![0xAB; 32])
        );

        let plain = stub.execute(&prepared).await.unwrap_err();
        assert!(plain.downcast_ref::<LandedUnread>().is_none());
    }

    #[tokio::test]
    async fn refuses_what_the_position_cannot_take_from_its_own_events() {
        let stub = stub();
        let req = request();
        let remove = |liquidity| LiquidityCommand::Remove {
            position: position(),
            liquidity,
            min_out: TokenPair::default(),
        };
        let close = LiquidityCommand::Close {
            position: position(),
        };
        let collect = LiquidityCommand::Collect {
            position: position(),
        };
        let err = stub.prepare(&close, &req).await.unwrap_err().to_string();
        assert!(err.contains("does not hold"), "{err}");

        stub.program_event(
            LiquidityEvent::Opened {
                position: position(),
                liquidity: 500,
                paid: TokenPair::new(10, 10),
            },
            1,
        );
        run(&stub, &open()).await;
        assert!(
            stub.prepare(&close, &req).await.is_err(),
            "it holds liquidity"
        );
        assert!(
            stub.prepare(&remove(501), &req).await.is_err(),
            "above its liquidity"
        );

        stub.program_event(
            LiquidityEvent::Removed {
                liquidity: 500,
                released: TokenPair::new(9, 9),
                transferred: TokenPair::default(),
            },
            2,
        );
        run(&stub, &remove(500)).await;
        assert!(
            stub.prepare(&close, &req).await.is_err(),
            "it owes what it released"
        );

        stub.program_event(
            LiquidityEvent::Collected {
                transferred: TokenPair::new(9, 9),
            },
            3,
        );
        run(&stub, &collect).await;
        stub.program_event(LiquidityEvent::Closed, 4);
        run(&stub, &close).await;
        assert!(
            stub.prepare(&close, &req).await.is_err(),
            "closed, so no longer held"
        );
    }

    #[tokio::test]
    async fn an_event_that_is_not_the_commands_is_an_error() {
        let stub = stub();
        stub.program_event(LiquidityEvent::Closed, 1);
        let prepared = stub.prepare(&open(), &request()).await.unwrap();
        let err = stub.execute(&prepared).await.unwrap_err().to_string();
        assert!(err.contains("Closed") && err.contains("Open"), "{err}");
    }

    #[tokio::test]
    async fn records_every_prepare_and_execute() {
        let stub = stub();
        stub.program_event(
            LiquidityEvent::Opened {
                position: position(),
                liquidity: 1,
                paid: TokenPair::new(1, 1),
            },
            1,
        );
        let prepared = stub.prepare(&open(), &request()).await.unwrap();
        stub.execute(&prepared).await.unwrap();
        let refused = LiquidityRequest {
            deadline_unix_secs: 0,
            ..request()
        };
        assert!(stub.prepare(&open(), &refused).await.is_err());

        let calls = stub.calls();
        assert_eq!(calls.len(), 3);
        assert!(matches!(
            &calls[0],
            LiquidityCall::Prepare { prepared: Some(p), .. } if p == &prepared
        ));
        assert!(matches!(
            &calls[1],
            LiquidityCall::Execute {
                command: Some(LiquidityCommand::Open { .. }),
                ..
            }
        ));
        assert!(matches!(
            &calls[2],
            LiquidityCall::Prepare { prepared: None, .. }
        ));
    }
}
