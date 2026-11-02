//! One connection that listens on behalf of every tenant a process serves.
//!
//! A replica of one tenant listens for its primary's announcements on a connection of its own. A
//! process that serves many tenants would need one per tenant, so it keeps one [`ListenHub`]
//! instead: a thread with one connection that `LISTEN`s on every channel some tenant asked for and
//! wakes that tenant's [`Subscription`] when its channel is announced. A missed announcement only
//! delays a reader, which looks on its own timer too, so the hub reconnects quietly after a failure.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// How long the hub's thread waits for a notification before it looks for channels to add or
/// drop. A new subscription is heard from within about this long.
const POLL: Duration = Duration::from_millis(200);

pub struct ListenHub {
    url: Arc<str>,
    channels: Mutex<HashMap<String, Vec<Weak<Signal>>>>,
    running: Mutex<bool>,
}

/// What a subscriber waits on: a count of announcements heard.
#[derive(Default)]
pub(crate) struct Signal {
    heard: Mutex<u64>,
    wake: Condvar,
}

/// A wait for one channel's announcements. Dropping it stops listening (the hub `UNLISTEN`s once
/// nobody waits on the channel).
pub struct Subscription {
    signal: Arc<Signal>,
    seen: Mutex<u64>,
    hub: Arc<ListenHub>,
}

impl Subscription {
    /// Waits until the channel is announced or `timeout` passes, and says whether it was. Any
    /// announcements since the last call count, so one that arrived between two waits is not lost.
    pub fn wait(&self, timeout: Duration) -> bool {
        self.hub.ensure_thread();
        let deadline = Instant::now() + timeout;
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        let mut heard = self.signal.heard.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if *heard != *seen {
                *seen = *heard;
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            heard = self
                .signal
                .wake
                .wait_timeout(heard, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
}

impl ListenHub {
    pub(crate) fn new(url: Arc<str>) -> Arc<Self> {
        Arc::new(Self {
            url,
            channels: Mutex::new(HashMap::new()),
            running: Mutex::new(false),
        })
    }

    pub(crate) fn subscribe(self: &Arc<Self>, channel: String) -> Subscription {
        let signal = Arc::new(Signal::default());
        self.channels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(channel)
            .or_default()
            .push(Arc::downgrade(&signal));
        self.ensure_thread();
        Subscription {
            signal,
            seen: Mutex::new(0),
            hub: self.clone(),
        }
    }

    fn ensure_thread(self: &Arc<Self>) {
        let mut running = self.running.lock().unwrap_or_else(|p| p.into_inner());
        if *running {
            return;
        }
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("refhir-listen".into())
            .spawn(move || run(weak));
        match spawned {
            Ok(_) => *running = true,
            Err(err) => {
                tracing::warn!(target: "refhir", "cannot start the listening thread: {err}")
            }
        }
    }

    /// The channels someone still waits on; forgets the subscribers that are gone.
    fn wanted(&self) -> Vec<String> {
        let mut channels = self.channels.lock().unwrap_or_else(|p| p.into_inner());
        channels.retain(|_, signals| {
            signals.retain(|signal| signal.strong_count() > 0);
            !signals.is_empty()
        });
        channels.keys().cloned().collect()
    }

    fn deliver(&self, channel: &str) {
        let channels = self.channels.lock().unwrap_or_else(|p| p.into_inner());
        for signal in channels.get(channel).into_iter().flatten() {
            if let Some(signal) = signal.upgrade() {
                *signal.heard.lock().unwrap_or_else(|p| p.into_inner()) += 1;
                signal.wake.notify_all();
            }
        }
    }
}

fn run(hub: Weak<ListenHub>) {
    let mut client: Option<crate::Conn> = None;
    let mut listening: Vec<String> = Vec::new();
    let mut failures: u32 = 0;
    loop {
        let Some(hub) = hub.upgrade() else {
            return;
        };
        if client.is_none() {
            match crate::connect_client(&hub.url) {
                Ok(connected) => {
                    client = Some(crate::Conn::new(connected));
                    listening.clear();
                    if failures > 0 {
                        tracing::info!(target: "refhir", "listening for publishes again");
                    }
                    failures = 0;
                }
                Err(err) => {
                    if failures == 0 {
                        tracing::warn!(target: "refhir", "cannot listen for publishes: {err}");
                    }
                    failures = failures.saturating_add(1);
                    drop(hub);
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }
        }
        let conn = client.as_mut().expect("connected above");
        let wanted = hub.wanted();
        let mut statements = String::new();
        for channel in &wanted {
            if !listening.contains(channel) {
                statements.push_str(&format!("LISTEN {channel}; "));
            }
        }
        for channel in &listening {
            if !wanted.contains(channel) {
                statements.push_str(&format!("UNLISTEN {channel}; "));
            }
        }
        if !statements.is_empty() {
            if let Err(err) = conn.batch_execute(&statements) {
                tracing::warn!(target: "refhir", "publish listener failed, reconnecting: {err}");
                client = None;
                continue;
            }
            listening = wanted;
            // A publish made before the LISTEN took effect is not heard; wake everyone once, so
            // that a new subscriber looks right away instead of after its own timer.
            for channel in &listening {
                hub.deliver(channel);
            }
        }
        let mut heard: Vec<String> = Vec::new();
        let outcome = {
            use postgres::fallible_iterator::FallibleIterator;
            let first = conn.notifications().timeout_iter(POLL).next();
            if let Ok(Some(notice)) = &first {
                heard.push(notice.channel().to_string());
                // Whatever else has arrived is read without waiting.
                let mut pending = conn.notifications();
                let mut drain = pending.iter();
                while let Ok(Some(notice)) = drain.next() {
                    heard.push(notice.channel().to_string());
                }
            }
            first.map(|_| ())
        };
        heard.sort();
        heard.dedup();
        for channel in &heard {
            hub.deliver(channel);
        }
        if let Err(err) = outcome {
            tracing::warn!(target: "refhir", "publish listener failed, reconnecting: {err}");
            client = None;
        }
    }
}
