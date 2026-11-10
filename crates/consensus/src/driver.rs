//! Threaded driver: owns a [`Core`] and its env on a dedicated thread,
//! feeds it network messages, executor feedback and timer ticks, and routes
//! its outputs to the network and to an output sink.

use crate::config::CoreConfig;
use crate::core::Core;
use crossbeam_channel::{select, Receiver, Sender};
use interfaces::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Control events for a running driver.
#[derive(Debug)]
pub enum DriverEvent {
    /// Executor finished `height` with state root `root`.
    Executed { height: Height, root: Hash32 },
    /// Persist a snapshot into the block store (served to lagging peers via
    /// the env's `snapshot()`; the env may read `store().latest_snapshot()`).
    SaveSnapshot(Snapshot),
    /// Extra inbound consensus message (alternative to the inbound channel).
    Message(Address, ConsensusMsg),
    Shutdown,
}

/// Live counters updated by the driver thread.
#[derive(Debug, Default)]
pub struct DriverStatus {
    pub committed_height: AtomicU64,
    pub view: AtomicU64,
    pub high_qc_view: AtomicU64,
    pub divergences: AtomicU64,
}

pub struct DriverHandle {
    tx: Sender<DriverEvent>,
    status: Arc<DriverStatus>,
    thread: Option<JoinHandle<()>>,
}

impl DriverHandle {
    /// Sender for control events (cloneable; give it to the executor thread).
    pub fn sender(&self) -> Sender<DriverEvent> {
        self.tx.clone()
    }
    pub fn executed(&self, height: Height, root: Hash32) {
        let _ = self.tx.send(DriverEvent::Executed { height, root });
    }
    pub fn status(&self) -> Arc<DriverStatus> {
        self.status.clone()
    }
    /// Stop the thread and wait for it.
    pub fn shutdown(mut self) {
        let _ = self.tx.send(DriverEvent::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for DriverHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(DriverEvent::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct Driver;

/// Longest the driver sleeps without ticking the core.
const MAX_IDLE: Duration = Duration::from_millis(100);

impl Driver {
    /// Spawn the consensus thread.
    ///
    /// * `env` — node services (wall clock, mempool, executor roots, signing,
    ///   block store). Constructed by the node; moved into the thread.
    /// * `network` — outbound transport (`Send` -> `Network::send`,
    ///   `Broadcast` -> `Network::broadcast`). Messages to self are handled
    ///   inside the core; the network must not loop broadcasts back (if it
    ///   does they are ignored).
    /// * `inbound` — authenticated `(from, ConsensusMsg)` from peers.
    /// * `sink` — receives `Commit` and `InstallSnapshot` outputs, in order,
    ///   on the consensus thread. Keep it cheap (hand off to the executor
    ///   via a channel).
    pub fn spawn<E>(
        cfg: CoreConfig,
        mut env: E,
        network: Arc<dyn Network>,
        inbound: Receiver<(Address, ConsensusMsg)>,
        mut sink: Box<dyn FnMut(ConsensusOutput) + Send>,
    ) -> DriverHandle
    where
        E: ConsensusEnv + Send + 'static,
    {
        let (tx, rx) = crossbeam_channel::unbounded::<DriverEvent>();
        let status = Arc::new(DriverStatus::default());
        let st = status.clone();
        let thread = std::thread::Builder::new()
            .name("consensus".into())
            .spawn(move || {
                let mut core = Core::new(cfg, &mut env);
                let mut next_timer: u64 = 0;
                let mut input = Some(ConsensusInput::Tick);
                loop {
                    if let Some(i) = input.take() {
                        let outs = core.handle(i, &mut env);
                        for o in outs {
                            match o {
                                ConsensusOutput::Send { to, msg } => network.send(to, NetMsg::Consensus(msg)),
                                ConsensusOutput::Broadcast(msg) => network.broadcast(NetMsg::Consensus(msg)),
                                ConsensusOutput::SetTimer { at_ms } => next_timer = at_ms,
                                o @ (ConsensusOutput::Commit(_) | ConsensusOutput::InstallSnapshot(_)) => sink(o),
                            }
                        }
                        st.committed_height.store(core.committed_height(), Ordering::Relaxed);
                        st.view.store(core.current_view(), Ordering::Relaxed);
                        st.high_qc_view.store(core.high_qc().data.view, Ordering::Relaxed);
                        st.divergences.store(core.stats().divergences, Ordering::Relaxed);
                    }
                    let now = env.now_ms();
                    let wait = if next_timer <= now {
                        Duration::ZERO
                    } else {
                        Duration::from_millis(next_timer - now).min(MAX_IDLE)
                    };
                    if wait.is_zero() {
                        // timer due: still drain one pending message first
                        if let Ok((from, msg)) = inbound.try_recv() {
                            input = Some(ConsensusInput::Message { from, msg });
                        } else {
                            input = Some(ConsensusInput::Tick);
                            next_timer = u64::MAX;
                        }
                        continue;
                    }
                    select! {
                        recv(inbound) -> m => match m {
                            Ok((from, msg)) => input = Some(ConsensusInput::Message { from, msg }),
                            Err(_) => return,
                        },
                        recv(rx) -> e => match e {
                            Ok(DriverEvent::Executed { height, root }) => input = Some(ConsensusInput::Executed { height, root }),
                            Ok(DriverEvent::Message(from, msg)) => input = Some(ConsensusInput::Message { from, msg }),
                            Ok(DriverEvent::SaveSnapshot(s)) => {
                                if let Err(e) = env.store().save_snapshot(&s) {
                                    tracing::error!(err = %e, "save_snapshot failed");
                                }
                            }
                            Ok(DriverEvent::Shutdown) | Err(_) => return,
                        },
                        default(wait) => input = Some(ConsensusInput::Tick),
                    }
                }
            })
            .expect("spawn consensus thread");
        DriverHandle { tx, status, thread: Some(thread) }
    }
}
