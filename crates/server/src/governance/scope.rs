//! Who is asking, and what has been done for them so far.
//!
//! Handlers do not see tokens, and the engine they call (`execute`) knows nothing of callers.
//! So that an operation can be attributed without threading a caller through forty handlers, the
//! request carries a [`RequestScope`] in a task-local for as long as it runs. Whatever sits
//! inside the scope can ask for [`current`].
//!
//! A task-local does not cross into `spawn_blocking`, which the OAuth handlers run in, so
//! [`in_blocking`] carries the scope across on a thread-local for the length of the closure.

use std::cell::RefCell;
use std::future::Future;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use super::limiter::Decision;
use super::Governance;

/// How the work reached the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Http,
    /// A message on a `/sync` websocket.
    Sync,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Http => "http",
            Source::Sync => "sync",
        }
    }
}

/// What a token says about its holder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub client_id: String,
    /// The `sub` of the user the token was issued for.
    pub user: Option<String>,
    pub fhir_user: Option<String>,
}

#[derive(Default)]
struct State {
    identity: Option<Identity>,
    audited: u32,
    throttled: bool,
    /// The caller's limits have been applied to this request.
    admitted: bool,
    /// A replica sent this request to the primary, which limits, meters and audits it as one
    /// of its own.
    forwarded: bool,
    decision: Option<Decision>,
}

/// One request, or one message on a long-lived connection.
pub struct RequestScope {
    pub request_id: String,
    pub source: Source,
    pub remote_ip: Option<IpAddr>,
    governance: Arc<Governance>,
    state: Mutex<State>,
}

impl RequestScope {
    pub(super) fn new(
        governance: Arc<Governance>,
        request_id: String,
        source: Source,
        remote_ip: Option<IpAddr>,
    ) -> Arc<Self> {
        Arc::new(Self {
            request_id,
            source,
            remote_ip,
            governance,
            state: Mutex::new(State::default()),
        })
    }

    pub fn governance(&self) -> &Arc<Governance> {
        &self.governance
    }

    /// Record who the token belongs to. Called as soon as it is known, so that a request that
    /// goes on to be refused is still attributed.
    pub fn set_identity(&self, identity: Identity) {
        self.state.lock().expect("scope").identity = Some(identity);
    }

    pub fn identity(&self) -> Option<Identity> {
        self.state.lock().expect("scope").identity.clone()
    }

    /// The caller as usage and audit rows name them: `client:<id>`, or `anonymous` for a request
    /// with no token. Not the address: a table keyed by every address that ever connected is a
    /// way to fill a disk.
    pub fn principal(&self) -> String {
        match &self.state.lock().expect("scope").identity {
            Some(identity) => format!("client:{}", identity.client_id),
            None => "anonymous".to_string(),
        }
    }

    /// The caller as the rate limiter tells them apart: a user of a client, or an address.
    pub fn limiter_key(&self) -> String {
        match &self.state.lock().expect("scope").identity {
            Some(identity) => format!(
                "client:{}|{}",
                identity.client_id,
                identity.user.as_deref().unwrap_or("-")
            ),
            None => ip_key(self.remote_ip),
        }
    }

    /// The engine has written audit rows for this request, so it needs no general one.
    pub fn note_audited(&self, rows: u32) {
        let mut state = self.state.lock().expect("scope");
        state.audited = state.audited.saturating_add(rows);
    }

    pub fn audited(&self) -> u32 {
        self.state.lock().expect("scope").audited
    }

    pub fn mark_throttled(&self) {
        self.state.lock().expect("scope").throttled = true;
    }

    pub fn throttled(&self) -> bool {
        self.state.lock().expect("scope").throttled
    }

    pub fn admitted(&self) -> bool {
        self.state.lock().expect("scope").admitted
    }

    pub fn mark_admitted(&self) {
        self.state.lock().expect("scope").admitted = true;
    }

    /// The request was sent on to the primary. It is still logged here, as a request this
    /// process received, but the primary counts and audits what it does: counted here too, a
    /// forwarded write would use up twice its quota and leave two audit rows.
    pub fn mark_forwarded(&self) {
        self.state.lock().expect("scope").forwarded = true;
    }

    pub fn forwarded(&self) -> bool {
        self.state.lock().expect("scope").forwarded
    }

    pub fn set_decision(&self, decision: Decision) {
        self.state.lock().expect("scope").decision = Some(decision);
    }

    pub fn decision(&self) -> Option<Decision> {
        self.state.lock().expect("scope").decision
    }

    /// A scope for one message on a connection that outlives the request that opened it: the
    /// same caller, a request of its own.
    pub fn message(&self) -> Arc<RequestScope> {
        let scope = RequestScope::new(
            self.governance.clone(),
            self.governance.new_request_id(),
            Source::Sync,
            self.remote_ip,
        );
        if let Some(identity) = self.identity() {
            scope.set_identity(identity);
        }
        scope
    }
}

/// An IPv4 address a dual-stack socket reports as IPv4-mapped IPv6 (`::ffff:203.0.113.9`),
/// as the IPv4 address it is. Left alone, every IPv4 client of a server bound to `[::]` would
/// share one /64.
pub fn unmapped(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(address)),
        other => other,
    }
}

/// The key a caller with no token is limited under. An IPv6 address is taken as its /64: a
/// single host is routinely given the whole of one, so each address in it is not a new caller.
pub fn ip_key(ip: Option<IpAddr>) -> String {
    match ip {
        None => "ip:unknown".to_string(),
        Some(IpAddr::V4(address)) => format!("ip:{address}"),
        Some(IpAddr::V6(address)) => {
            let mut octets = address.octets();
            octets[8..].fill(0);
            format!("ip:{}/64", std::net::Ipv6Addr::from(octets))
        }
    }
}

tokio::task_local! {
    static SCOPE: Arc<RequestScope>;
}

thread_local! {
    static BLOCKING: RefCell<Option<Arc<RequestScope>>> = const { RefCell::new(None) };
}

/// The scope of the request this code is running for, if there is one. There is none where
/// governance is off, and none for work no request asked for, such as a background job.
pub fn current() -> Option<Arc<RequestScope>> {
    SCOPE
        .try_with(Arc::clone)
        .ok()
        .or_else(|| BLOCKING.with(|slot| slot.borrow().clone()))
}

/// Run `work` with `scope` as the current one.
pub async fn within<F: Future>(scope: Arc<RequestScope>, work: F) -> F::Output {
    SCOPE.scope(scope, work).await
}

/// Run blocking `work` with `scope` as the current one. Call it inside the closure given to
/// `spawn_blocking`.
pub fn in_blocking<T>(scope: Option<Arc<RequestScope>>, work: impl FnOnce() -> T) -> T {
    /// Puts the previous scope back even if the work panics: tokio reuses its blocking threads,
    /// and a scope left behind would be taken for the next job's.
    struct Restore(Option<Arc<RequestScope>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            BLOCKING.with(|slot| *slot.borrow_mut() = previous);
        }
    }
    let _restore = Restore(BLOCKING.with(|slot| slot.replace(scope)));
    work()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ipv4_client_of_a_dual_stack_socket_is_an_ipv4_client() {
        let mapped: IpAddr = "::ffff:203.0.113.9".parse().unwrap();
        assert_eq!(unmapped(mapped), "203.0.113.9".parse::<IpAddr>().unwrap());
        assert_eq!(ip_key(Some(unmapped(mapped))), "ip:203.0.113.9");
        // Without this every IPv4 client would share the key of the all-zero /64.
        assert_eq!(ip_key(Some(mapped)), "ip:::/64");
        let native: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(unmapped(native), native);
    }

    #[test]
    fn a_scope_does_not_outlive_the_work_it_was_lent_to_even_if_that_panics() {
        let governance =
            super::super::Governance::new(super::super::GovernanceConfig::observing(), false, None);
        let scope = RequestScope::new(governance, "r".into(), Source::Http, None);
        assert!(current().is_none());
        let inner = scope.clone();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            in_blocking(Some(inner), || {
                assert_eq!(current().unwrap().request_id, "r");
                panic!("the work failed");
            })
        }));
        assert!(caught.is_err());
        assert!(
            current().is_none(),
            "not left behind for the next job on this thread"
        );
        assert!(in_blocking(Some(scope), || current().is_some()));
        assert!(current().is_none());
    }

    #[test]
    fn addresses_are_keyed_so_that_a_hosts_ipv6_range_is_one_caller() {
        assert_eq!(
            ip_key(Some("203.0.113.9".parse().unwrap())),
            "ip:203.0.113.9"
        );
        assert_eq!(
            ip_key(Some("2001:db8:1:2:aaaa:bbbb:cccc:dddd".parse().unwrap())),
            "ip:2001:db8:1:2::/64"
        );
        assert_eq!(
            ip_key(Some("2001:db8:1:2:1::1".parse().unwrap())),
            ip_key(Some("2001:db8:1:2:2::2".parse().unwrap()))
        );
        assert_eq!(ip_key(None), "ip:unknown");
    }
}
