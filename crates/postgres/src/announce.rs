//! Announcing a publish to the replicas that listen for it, at most once per interval.
//!
//! Postgres serializes the commit of every transaction that runs `NOTIFY` across the whole cluster
//! (every database in it), so a cluster of busy writers spends its commits queueing on one lock if
//! each of them announces every publish. An announcement only wakes a replica, which then reads the
//! published watermark, and the watermark is written every time, so announcing less often costs
//! nothing but replica latency inside a burst: the first publish after a quiet spell is announced
//! by the commit that records it (no added delay), and the publishes that follow within
//! `interval` are announced once, by a thread, when the interval has passed.
//!
//! A replica that misses an announcement still looks on its own timer, so a failure here delays
//! it and never loses a commit.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use postgres::Client;

/// How often an idle thread looks again whether its owner is still there.
const IDLE: Duration = Duration::from_millis(500);

pub(crate) struct Announcer {
    /// Zero announces every publish, with the commit that records it.
    interval: Duration,
    channel: String,
    url: Arc<str>,
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    /// When an announcement was last made, or taken for the commit that records a publish.
    last: Option<Instant>,
    /// A publish since `last` is recorded and not yet announced.
    owed: bool,
    /// The thread that makes owed announcements is running.
    running: bool,
}

fn locked(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Announcer {
    pub(crate) fn new(interval: Duration, channel: String, url: Arc<str>) -> Arc<Self> {
        Arc::new(Self {
            interval,
            channel,
            url,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        })
    }

    pub(crate) fn channel(&self) -> &str {
        &self.channel
    }

    /// Whether the commit that records a publish should announce it. When it should not, the
    /// publish is owed an announcement: call [`Announcer::owe`] once it has committed.
    pub(crate) fn announce_with_commit(self: &Arc<Self>) -> bool {
        if self.interval.is_zero() {
            return true;
        }
        let mut state = locked(&self.state);
        let now = Instant::now();
        match state.last {
            Some(last) if now.duration_since(last) < self.interval => false,
            _ => {
                state.last = Some(now);
                state.owed = false;
                // The thread connects as it starts, so by the time a burst that follows this
                // publish is due its announcement the connection is made.
                self.ensure_thread(&mut state);
                true
            }
        }
    }

    /// A publish is recorded and has not been announced: have the thread announce it when the
    /// interval allows. Called after the commit that recorded it, so the announcement is never
    /// heard before the watermark it is about.
    pub(crate) fn owe(self: &Arc<Self>) {
        if self.interval.is_zero() {
            return;
        }
        let mut state = locked(&self.state);
        state.owed = true;
        self.ensure_thread(&mut state);
        self.wake.notify_all();
    }

    fn ensure_thread(self: &Arc<Self>, state: &mut State) {
        if state.running {
            return;
        }
        state.running = true;
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("refhir-announce".into())
            .spawn(move || run(weak));
        if let Err(err) = spawned {
            state.running = false;
            tracing::warn!(target: "refhir", "cannot start the announcement thread: {err}");
        }
    }

    fn send(&self, client: &mut Option<Client>) -> Result<(), String> {
        if client.is_none() {
            *client = Some(crate::connect_client(&self.url).map_err(|err| err.to_string())?);
        }
        let connected = client.as_mut().expect("connected above");
        connected
            .batch_execute(&format!("NOTIFY {}", self.channel))
            .map_err(|err| err.to_string())
    }
}

/// Makes the owed announcements until the persistence that owns the announcer is gone.
fn run(announcer: std::sync::Weak<Announcer>) {
    let mut client: Option<Client> = announcer
        .upgrade()
        .and_then(|announcer| crate::connect_client(&announcer.url).ok());
    loop {
        // Held for one pass only, so the thread never keeps its owner alive for long.
        let Some(announcer) = announcer.upgrade() else {
            return;
        };
        let due = {
            let mut state = locked(&announcer.state);
            let wait = match (state.owed, state.last) {
                (false, _) => Some(IDLE),
                (true, Some(last)) => announcer
                    .interval
                    .checked_sub(last.elapsed())
                    .filter(|left| !left.is_zero()),
                (true, None) => None,
            };
            match wait {
                Some(wait) => {
                    drop(
                        announcer
                            .wake
                            .wait_timeout(state, wait)
                            .unwrap_or_else(|poisoned| poisoned.into_inner()),
                    );
                    false
                }
                None => {
                    state.owed = false;
                    state.last = Some(Instant::now());
                    true
                }
            }
        };
        if !due {
            continue;
        }
        if let Err(err) = announcer.send(&mut client) {
            tracing::warn!(target: "refhir", "cannot announce a publish: {err}");
            client = None;
            locked(&announcer.state).owed = true;
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
