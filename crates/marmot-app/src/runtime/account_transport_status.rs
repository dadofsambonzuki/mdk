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
        self.sender(account_id).borrow().clone()
    }

    pub(crate) fn publish(
        &self,
        account_id: &MemberId,
        mut snapshot: AccountTransportStatusSnapshot,
    ) -> bool {
        let sender = self.sender(account_id);
        let current = sender.borrow().clone();
        if current.semantic_eq(&snapshot) {
            return false;
        }
        snapshot.revision = current.revision.saturating_add(1);
        sender.send_replace(snapshot);
        true
    }

    pub(crate) fn mark_inactive(&self, account_id: &MemberId) {
        let _ = self.publish(account_id, AccountTransportStatusSnapshot::inactive());
    }

    pub(crate) fn mark_replay_complete(&self, account_id: &MemberId) {
        let mut snapshot = self.snapshot(account_id);
        if snapshot.state == AccountTransportState::Inactive {
            return;
        }
        if let Some(inbox) = &mut snapshot.inbox {
            inbox.pending_replay = false;
        }
        for route in snapshot
            .current_group_routes
            .iter_mut()
            .chain(snapshot.historical_group_routes.iter_mut())
        {
            route.pending_replay = false;
        }
        let _ = self.publish(account_id, snapshot);
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
}
