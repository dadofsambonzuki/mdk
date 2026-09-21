//! Read-only account transport coverage snapshots.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use cgka_traits::MemberId;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountTransportState {
    Inactive,
    Available,
    Degraded,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountTransportRouteRole {
    Inbox,
    CurrentGroup,
    HistoricalGroup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountTransportRouteState {
    Pending,
    Registered,
    RetryPending,
    PolicyBlocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationDetailCompleteness {
    Exact,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointAdmissionOutcome {
    Allowed,
    Invalid,
    Unsafe,
    Retired,
    Duplicate,
    BeyondRouteLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointRegistrationOutcome {
    NotAttempted,
    Pending,
    Registered,
    Failed,
    Unknown,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountTransportEndpointStatus {
    pub requested_endpoint: String,
    pub normalized_endpoint: Option<String>,
    pub admission: EndpointAdmissionOutcome,
    pub registration: EndpointRegistrationOutcome,
}

impl fmt::Debug for AccountTransportEndpointStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountTransportEndpointStatus")
            .field("admission", &self.admission)
            .field("registration", &self.registration)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountTransportRouteStatus {
    /// Opaque process-local reference suitable for a repair action.
    pub route_ref: String,
    /// Caller-owned MLS group id; variable length and absent for the inbox.
    pub group_id_hex: Option<String>,
    /// Nostr routing handle; absent for the inbox.
    pub transport_group_id_hex: Option<String>,
    pub role: AccountTransportRouteRole,
    pub state: AccountTransportRouteState,
    pub requested_endpoint_count: u32,
    pub admitted_endpoint_count: u32,
    /// `None` when a compatibility client cannot report exact endpoint coverage.
    pub registered_endpoint_count: Option<u32>,
    pub registration_detail: RegistrationDetailCompleteness,
    pub endpoints: Vec<AccountTransportEndpointStatus>,
    pub pending_registration: bool,
    /// Requested history coverage remains open. For a policy-excluded route
    /// this can describe a dormant durable gap, not active network replay.
    pub pending_replay: bool,
    /// Selected retry delay, not a live countdown.
    pub retry_delay_ms: Option<u64>,
}

impl fmt::Debug for AccountTransportRouteStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountTransportRouteStatus")
            .field("role", &self.role)
            .field("state", &self.state)
            .field("requested_endpoint_count", &self.requested_endpoint_count)
            .field("admitted_endpoint_count", &self.admitted_endpoint_count)
            .field("registered_endpoint_count", &self.registered_endpoint_count)
            .field("registration_detail", &self.registration_detail)
            .field("pending_registration", &self.pending_registration)
            .field("pending_replay", &self.pending_replay)
            .field("retry_delay_ms", &self.retry_delay_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountTransportStatusSnapshot {
    pub revision: u64,
    pub state: AccountTransportState,
    pub inbox: Option<AccountTransportRouteStatus>,
    pub current_group_routes: Vec<AccountTransportRouteStatus>,
    pub historical_group_routes: Vec<AccountTransportRouteStatus>,
}

impl AccountTransportStatusSnapshot {
    pub(crate) fn inactive() -> Self {
        Self {
            revision: 0,
            state: AccountTransportState::Inactive,
            inbox: None,
            current_group_routes: Vec::new(),
            historical_group_routes: Vec::new(),
        }
    }

    pub(crate) fn semantic_eq(&self, other: &Self) -> bool {
        self.state == other.state
            && self.inbox == other.inbox
            && self.current_group_routes == other.current_group_routes
            && self.historical_group_routes == other.historical_group_routes
    }
}

impl fmt::Debug for AccountTransportStatusSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountTransportStatusSnapshot")
            .field("revision", &self.revision)
            .field("state", &self.state)
            .field("has_inbox", &self.inbox.is_some())
            .field(
                "current_group_route_count",
                &self.current_group_routes.len(),
            )
            .field(
                "historical_group_route_count",
                &self.historical_group_routes.len(),
            )
            .finish()
    }
}

#[derive(Clone, Default)]
pub(crate) struct AccountTransportStatusRegistry {
    inner: Arc<Mutex<HashMap<MemberId, watch::Sender<AccountTransportStatusSnapshot>>>>,
}

impl AccountTransportStatusRegistry {
    fn sender(&self, account_id: &MemberId) -> watch::Sender<AccountTransportStatusSnapshot> {
        let mut entries = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries
            .entry(account_id.clone())
            .or_insert_with(|| watch::channel(AccountTransportStatusSnapshot::inactive()).0)
            .clone()
    }

    pub(crate) fn snapshot(&self, account_id: &MemberId) -> AccountTransportStatusSnapshot {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(account_id)
            .map(|sender| sender.borrow().clone())
            .unwrap_or_else(AccountTransportStatusSnapshot::inactive)
    }

    pub(crate) fn publish(
        &self,
        account_id: &MemberId,
        mut snapshot: AccountTransportStatusSnapshot,
    ) -> bool {
        let mut entries = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sender = entries
            .entry(account_id.clone())
            .or_insert_with(|| watch::channel(AccountTransportStatusSnapshot::inactive()).0);
        let current = sender.borrow().clone();
        if current.semantic_eq(&snapshot) {
            return false;
        }
        snapshot.revision = current.revision.saturating_add(1);
        sender.send_replace(snapshot);
        true
    }

    /// Apply a status-only mutation while the registry entry remains active.
    /// Holding the registry lock across the read/modify/write prevents a stale
    /// snapshot from overwriting a concurrent terminal `Inactive` transition.
    pub(crate) fn update_active(
        &self,
        account_id: &MemberId,
        update: impl FnOnce(&mut AccountTransportStatusSnapshot),
    ) -> bool {
        let entries = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(sender) = entries.get(account_id) else {
            return false;
        };
        let current = sender.borrow().clone();
        if current.state == AccountTransportState::Inactive {
            return false;
        }
        let mut next = current.clone();
        update(&mut next);
        if current.semantic_eq(&next) {
            return false;
        }
        next.revision = current.revision.saturating_add(1);
        sender.send_replace(next);
        true
    }

    /// Publish a snapshot only if no other status transition has advanced the
    /// entry since the caller took its source snapshot. The current snapshot
    /// is returned on a stale compare so callers can preserve terminal state.
    pub(crate) fn publish_if_revision(
        &self,
        account_id: &MemberId,
        expected_revision: u64,
        mut snapshot: AccountTransportStatusSnapshot,
    ) -> Result<bool, Box<AccountTransportStatusSnapshot>> {
        let mut entries = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sender = entries
            .entry(account_id.clone())
            .or_insert_with(|| watch::channel(AccountTransportStatusSnapshot::inactive()).0);
        let current = sender.borrow().clone();
        if current.revision != expected_revision {
            return Err(Box::new(current));
        }
        if current.semantic_eq(&snapshot) {
            return Ok(false);
        }
        snapshot.revision = current.revision.saturating_add(1);
        sender.send_replace(snapshot);
        Ok(true)
    }

    pub(crate) fn mark_inactive(&self, account_id: &MemberId) {
        let _ = self.publish(account_id, AccountTransportStatusSnapshot::inactive());
    }

    pub(crate) fn remove(&self, account_id: &MemberId) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(account_id);
    }

    fn subscribe(
        &self,
        account_id: &MemberId,
    ) -> (
        AccountTransportStatusSnapshot,
        watch::Receiver<AccountTransportStatusSnapshot>,
    ) {
        let sender = self.sender(account_id);
        let receiver = sender.subscribe();
        let snapshot = receiver.borrow().clone();
        (snapshot, receiver)
    }
}

/// Bounded, coalescing subscription to complete account transport snapshots.
pub struct RuntimeAccountTransportStatusSubscription {
    pub snapshot: AccountTransportStatusSnapshot,
    updates: watch::Receiver<AccountTransportStatusSnapshot>,
    stopping: watch::Receiver<bool>,
}

impl RuntimeAccountTransportStatusSubscription {
    pub(crate) fn new(
        registry: &AccountTransportStatusRegistry,
        account_id: &MemberId,
        stopping: watch::Receiver<bool>,
    ) -> Self {
        let (snapshot, updates) = registry.subscribe(account_id);
        Self {
            snapshot,
            updates,
            stopping,
        }
    }

    pub async fn recv(&mut self) -> Option<AccountTransportStatusSnapshot> {
        if *self.stopping.borrow() {
            return None;
        }
        tokio::select! {
            biased;
            changed = self.stopping.changed() => {
                let _ = changed;
                None
            }
            changed = self.updates.changed() => {
                changed.ok()?;
                Some(self.updates.borrow_and_update().clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_equality_ignores_process_local_revision() {
        let mut left = AccountTransportStatusSnapshot::inactive();
        let mut right = left.clone();
        left.revision = 7;
        right.revision = 11;

        assert!(left.semantic_eq(&right));
        assert_ne!(left, right);
    }

    #[test]
    fn debug_output_does_not_expose_route_or_endpoint_identifiers() {
        let endpoint = AccountTransportEndpointStatus {
            requested_endpoint: "wss://private-route.example".into(),
            normalized_endpoint: Some("wss://private-route.example/".into()),
            admission: EndpointAdmissionOutcome::Allowed,
            registration: EndpointRegistrationOutcome::Registered,
        };
        let route = AccountTransportRouteStatus {
            route_ref: "opaque-secret".into(),
            group_id_hex: Some("aabb".into()),
            transport_group_id_hex: Some("ccdd".into()),
            role: AccountTransportRouteRole::CurrentGroup,
            state: AccountTransportRouteState::Registered,
            requested_endpoint_count: 1,
            admitted_endpoint_count: 1,
            registered_endpoint_count: Some(1),
            registration_detail: RegistrationDetailCompleteness::Exact,
            endpoints: vec![endpoint.clone()],
            pending_registration: false,
            pending_replay: true,
            retry_delay_ms: None,
        };

        let debug = format!("{endpoint:?} {route:?}");
        for secret in ["private-route.example", "opaque-secret", "aabb", "ccdd"] {
            assert!(!debug.contains(secret));
        }
    }

    #[tokio::test]
    async fn registry_coalesces_and_only_advances_revision_on_semantic_change() {
        let registry = AccountTransportStatusRegistry::default();
        let account_id = MemberId::new(vec![0xAA; 32]);
        let (_stopping_sender, stopping) = watch::channel(false);
        let mut subscription =
            RuntimeAccountTransportStatusSubscription::new(&registry, &account_id, stopping);
        assert_eq!(subscription.snapshot.state, AccountTransportState::Inactive);

        let mut unavailable = AccountTransportStatusSnapshot::inactive();
        unavailable.state = AccountTransportState::Unavailable;
        assert!(registry.publish(&account_id, unavailable.clone()));
        assert!(!registry.publish(&account_id, unavailable));

        let mut available = AccountTransportStatusSnapshot::inactive();
        available.state = AccountTransportState::Available;
        assert!(registry.publish(&account_id, available));

        let update = subscription.recv().await.expect("latest update");
        assert_eq!(update.revision, 2);
        assert_eq!(update.state, AccountTransportState::Available);
    }

    #[tokio::test]
    async fn registry_removal_closes_subscriptions_and_drops_saved_status() {
        let registry = AccountTransportStatusRegistry::default();
        let account_id = MemberId::new(vec![0xBB; 32]);
        let (_stopping_sender, stopping) = watch::channel(false);
        let mut subscription =
            RuntimeAccountTransportStatusSubscription::new(&registry, &account_id, stopping);
        let mut available = AccountTransportStatusSnapshot::inactive();
        available.state = AccountTransportState::Available;
        assert!(registry.publish(&account_id, available));
        assert_eq!(
            subscription.recv().await.unwrap().state,
            AccountTransportState::Available
        );

        registry.remove(&account_id);

        assert!(subscription.recv().await.is_none());
        let recreated = registry.snapshot(&account_id);
        assert_eq!(recreated.state, AccountTransportState::Inactive);
        assert_eq!(recreated.revision, 0);
    }

    #[test]
    fn snapshot_of_unknown_account_does_not_allocate_a_registry_entry() {
        let registry = AccountTransportStatusRegistry::default();
        let account_id = MemberId::new(vec![0xCC; 32]);

        assert_eq!(
            registry.snapshot(&account_id).state,
            AccountTransportState::Inactive
        );
        assert!(
            registry
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
    }

    #[test]
    fn active_update_cannot_resurrect_a_newer_inactive_state() {
        let registry = AccountTransportStatusRegistry::default();
        let account_id = MemberId::new(vec![0xDD; 32]);
        let mut available = AccountTransportStatusSnapshot::inactive();
        available.state = AccountTransportState::Available;
        assert!(registry.publish(&account_id, available));

        registry.mark_inactive(&account_id);
        assert!(!registry.update_active(&account_id, |snapshot| {
            snapshot.state = AccountTransportState::Available;
        }));
        assert_eq!(
            registry.snapshot(&account_id).state,
            AccountTransportState::Inactive
        );
    }

    #[test]
    fn revision_fenced_publish_cannot_overwrite_newer_inactive_state() {
        let registry = AccountTransportStatusRegistry::default();
        let account_id = MemberId::new(vec![0xEE; 32]);
        let mut unavailable = AccountTransportStatusSnapshot::inactive();
        unavailable.state = AccountTransportState::Unavailable;
        assert!(registry.publish(&account_id, unavailable));
        let stale = registry.snapshot(&account_id);
        let mut active = stale.clone();
        active.state = AccountTransportState::Available;

        registry.mark_inactive(&account_id);
        assert!(
            registry
                .publish_if_revision(&account_id, stale.revision, active)
                .is_err()
        );
        assert_eq!(
            registry.snapshot(&account_id).state,
            AccountTransportState::Inactive
        );
    }
}
