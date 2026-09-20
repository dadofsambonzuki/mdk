//! Account transport-route coverage exposed to native hosts.

use marmot_app as app;

macro_rules! ffi_enum {
    ($ffi:ident, $app:ident, $($variant:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
        pub enum $ffi { $($variant),+ }

        impl From<app::$app> for $ffi {
            fn from(value: app::$app) -> Self {
                match value { $(app::$app::$variant => Self::$variant),+ }
            }
        }
    };
}

ffi_enum!(
    AccountTransportStateFfi,
    AccountTransportState,
    Inactive,
    Available,
    Degraded,
    Unavailable,
);
ffi_enum!(
    AccountTransportRouteRoleFfi,
    AccountTransportRouteRole,
    Inbox,
    CurrentGroup,
    HistoricalGroup,
);
ffi_enum!(
    AccountTransportRouteStateFfi,
    AccountTransportRouteState,
    Pending,
    Registered,
    RetryPending,
    PolicyBlocked,
);
ffi_enum!(
    RegistrationDetailCompletenessFfi,
    RegistrationDetailCompleteness,
    Exact,
    Unknown,
);
ffi_enum!(
    EndpointAdmissionOutcomeFfi,
    EndpointAdmissionOutcome,
    Allowed,
    Invalid,
    Unsafe,
    Retired,
    Duplicate,
    BeyondRouteLimit,
);
ffi_enum!(
    EndpointRegistrationOutcomeFfi,
    EndpointRegistrationOutcome,
    NotAttempted,
    Pending,
    Registered,
    Failed,
    Unknown,
);

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AccountTransportEndpointStatusFfi {
    pub requested_endpoint: String,
    pub normalized_endpoint: Option<String>,
    pub admission: EndpointAdmissionOutcomeFfi,
    pub registration: EndpointRegistrationOutcomeFfi,
}

impl From<app::AccountTransportEndpointStatus> for AccountTransportEndpointStatusFfi {
    fn from(value: app::AccountTransportEndpointStatus) -> Self {
        Self {
            requested_endpoint: value.requested_endpoint,
            normalized_endpoint: value.normalized_endpoint,
            admission: value.admission.into(),
            registration: value.registration.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AccountTransportRouteStatusFfi {
    /// Opaque process-local reference suitable for a repair action.
    pub route_ref: String,
    /// Variable-length MLS group id, absent for the account inbox.
    pub group_id_hex: Option<String>,
    /// Nostr routing handle, absent for the account inbox.
    pub transport_group_id_hex: Option<String>,
    pub role: AccountTransportRouteRoleFfi,
    pub state: AccountTransportRouteStateFfi,
    pub requested_endpoint_count: u32,
    pub admitted_endpoint_count: u32,
    /// Absent when the relay client cannot report exact endpoint coverage.
    pub registered_endpoint_count: Option<u32>,
    pub registration_detail: RegistrationDetailCompletenessFfi,
    pub endpoints: Vec<AccountTransportEndpointStatusFfi>,
    pub pending_registration: bool,
    pub pending_replay: bool,
    /// Selected retry delay, not a live countdown.
    pub retry_delay_ms: Option<u64>,
}

impl From<app::AccountTransportRouteStatus> for AccountTransportRouteStatusFfi {
    fn from(value: app::AccountTransportRouteStatus) -> Self {
        Self {
            route_ref: value.route_ref,
            group_id_hex: value.group_id_hex,
            transport_group_id_hex: value.transport_group_id_hex,
            role: value.role.into(),
            state: value.state.into(),
            requested_endpoint_count: value.requested_endpoint_count,
            admitted_endpoint_count: value.admitted_endpoint_count,
            registered_endpoint_count: value.registered_endpoint_count,
            registration_detail: value.registration_detail.into(),
            endpoints: value.endpoints.into_iter().map(Into::into).collect(),
            pending_registration: value.pending_registration,
            pending_replay: value.pending_replay,
            retry_delay_ms: value.retry_delay_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AccountTransportStatusSnapshotFfi {
    /// Process-local semantic revision. It may jump when updates coalesce.
    pub revision: u64,
    pub state: AccountTransportStateFfi,
    pub inbox: Option<AccountTransportRouteStatusFfi>,
    pub current_group_routes: Vec<AccountTransportRouteStatusFfi>,
    pub historical_group_routes: Vec<AccountTransportRouteStatusFfi>,
}

impl From<app::AccountTransportStatusSnapshot> for AccountTransportStatusSnapshotFfi {
    fn from(value: app::AccountTransportStatusSnapshot) -> Self {
        Self {
            revision: value.revision,
            state: value.state.into(),
            inbox: value.inbox.map(Into::into),
            current_group_routes: value
                .current_group_routes
                .into_iter()
                .map(Into::into)
                .collect(),
            historical_group_routes: value
                .historical_group_routes
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(
        admission: app::EndpointAdmissionOutcome,
        registration: app::EndpointRegistrationOutcome,
    ) -> app::AccountTransportEndpointStatus {
        app::AccountTransportEndpointStatus {
            requested_endpoint: "wss://relay.example".into(),
            normalized_endpoint: Some("wss://relay.example/".into()),
            admission,
            registration,
        }
    }

    #[test]
    fn conversion_preserves_every_typed_outcome_and_optional_count() {
        let admissions = [
            app::EndpointAdmissionOutcome::Allowed,
            app::EndpointAdmissionOutcome::Invalid,
            app::EndpointAdmissionOutcome::Unsafe,
            app::EndpointAdmissionOutcome::Retired,
            app::EndpointAdmissionOutcome::Duplicate,
            app::EndpointAdmissionOutcome::BeyondRouteLimit,
        ];
        let registrations = [
            app::EndpointRegistrationOutcome::NotAttempted,
            app::EndpointRegistrationOutcome::Pending,
            app::EndpointRegistrationOutcome::Registered,
            app::EndpointRegistrationOutcome::Failed,
            app::EndpointRegistrationOutcome::Unknown,
        ];
        let endpoints = admissions
            .into_iter()
            .zip(registrations.into_iter().cycle())
            .map(|(admission, registration)| endpoint(admission, registration))
            .collect::<Vec<_>>();
        let route = app::AccountTransportRouteStatus {
            route_ref: "opaque".into(),
            group_id_hex: Some("abcd".into()),
            transport_group_id_hex: Some("ef".repeat(32)),
            role: app::AccountTransportRouteRole::HistoricalGroup,
            state: app::AccountTransportRouteState::RetryPending,
            requested_endpoint_count: 6,
            admitted_endpoint_count: 2,
            registered_endpoint_count: None,
            registration_detail: app::RegistrationDetailCompleteness::Unknown,
            endpoints,
            pending_registration: true,
            pending_replay: true,
            retry_delay_ms: Some(60_000),
        };
        let converted = AccountTransportRouteStatusFfi::from(route);

        assert_eq!(
            converted.role,
            AccountTransportRouteRoleFfi::HistoricalGroup
        );
        assert_eq!(converted.state, AccountTransportRouteStateFfi::RetryPending);
        assert_eq!(
            converted.registration_detail,
            RegistrationDetailCompletenessFfi::Unknown
        );
        assert_eq!(converted.registered_endpoint_count, None);
        assert_eq!(converted.retry_delay_ms, Some(60_000));
        assert_eq!(converted.endpoints.len(), 6);
        assert_eq!(
            converted.endpoints[4].admission,
            EndpointAdmissionOutcomeFfi::Duplicate
        );
        assert_eq!(
            converted.endpoints[4].registration,
            EndpointRegistrationOutcomeFfi::Unknown
        );
    }

    #[test]
    fn snapshot_conversion_preserves_route_partitions() {
        let route = app::AccountTransportRouteStatus {
            route_ref: "inbox".into(),
            group_id_hex: None,
            transport_group_id_hex: None,
            role: app::AccountTransportRouteRole::Inbox,
            state: app::AccountTransportRouteState::Registered,
            requested_endpoint_count: 1,
            admitted_endpoint_count: 1,
            registered_endpoint_count: Some(1),
            registration_detail: app::RegistrationDetailCompleteness::Exact,
            endpoints: vec![endpoint(
                app::EndpointAdmissionOutcome::Allowed,
                app::EndpointRegistrationOutcome::Registered,
            )],
            pending_registration: false,
            pending_replay: false,
            retry_delay_ms: None,
        };
        let converted =
            AccountTransportStatusSnapshotFfi::from(app::AccountTransportStatusSnapshot {
                revision: 9,
                state: app::AccountTransportState::Degraded,
                inbox: Some(route.clone()),
                current_group_routes: vec![route.clone()],
                historical_group_routes: vec![route],
            });

        assert_eq!(converted.revision, 9);
        assert_eq!(converted.state, AccountTransportStateFfi::Degraded);
        assert!(converted.inbox.is_some());
        assert_eq!(converted.current_group_routes.len(), 1);
        assert_eq!(converted.historical_group_routes.len(), 1);
    }
}
