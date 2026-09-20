//! C mirrors for complete account transport-route status snapshots.

use crate::macros::{c_enum, c_mirror};
use marmot_uniffi::conversions::*;

c_enum! {
    MarmotAccountTransportState from AccountTransportStateFfi {
        Inactive, Available, Degraded, Unavailable,
    }
}
c_enum! {
    MarmotAccountTransportRouteRole from AccountTransportRouteRoleFfi {
        Inbox, CurrentGroup, HistoricalGroup,
    }
}
c_enum! {
    MarmotAccountTransportRouteState from AccountTransportRouteStateFfi {
        Pending, Registered, RetryPending, PolicyBlocked,
    }
}
c_enum! {
    MarmotRegistrationDetailCompleteness from RegistrationDetailCompletenessFfi {
        Exact, Unknown,
    }
}
c_enum! {
    MarmotEndpointAdmissionOutcome from EndpointAdmissionOutcomeFfi {
        Allowed, Invalid, Unsafe, Retired, Duplicate, BeyondRouteLimit,
    }
}
c_enum! {
    MarmotEndpointRegistrationOutcome from EndpointRegistrationOutcomeFfi {
        NotAttempted, Pending, Registered, Failed, Unknown,
    }
}

c_mirror! {
    /// One requested endpoint's local admission and registration result.
    MarmotAccountTransportEndpointStatus from AccountTransportEndpointStatusFfi {
        str requested_endpoint,
        opt_str normalized_endpoint,
        copy admission: MarmotEndpointAdmissionOutcome,
        copy registration: MarmotEndpointRegistrationOutcome,
    }
}

c_mirror! {
    /// One desired inbox or group route. Owned by its enclosing snapshot.
    MarmotAccountTransportRouteStatus from AccountTransportRouteStatusFfi {
        str route_ref,
        opt_str group_id_hex,
        opt_str transport_group_id_hex,
        copy role: MarmotAccountTransportRouteRole,
        copy state: MarmotAccountTransportRouteState,
        copy requested_endpoint_count: u32,
        copy admitted_endpoint_count: u32,
        opt_copy has_registered_endpoint_count/registered_endpoint_count: u32,
        copy registration_detail: MarmotRegistrationDetailCompleteness,
        vec endpoints/endpoints_len: MarmotAccountTransportEndpointStatus,
        copy pending_registration: bool,
        copy pending_replay: bool,
        opt_copy has_retry_delay_ms/retry_delay_ms: u64,
    }
}

c_mirror! {
    /// Complete replacement for one account. Free only through this root.
    MarmotAccountTransportStatusSnapshot from AccountTransportStatusSnapshotFfi,
    free marmot_account_transport_status_snapshot_free {
        copy revision: u64,
        copy state: MarmotAccountTransportState,
        opt_rec inbox: MarmotAccountTransportRouteStatus,
        vec current_group_routes/current_group_routes_len: MarmotAccountTransportRouteStatus,
        vec historical_group_routes/historical_group_routes_len: MarmotAccountTransportRouteStatus,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{audit, boxed};

    fn endpoint(
        admission: EndpointAdmissionOutcomeFfi,
        registration: EndpointRegistrationOutcomeFfi,
    ) -> AccountTransportEndpointStatusFfi {
        AccountTransportEndpointStatusFfi {
            requested_endpoint: "wss://relay.example".into(),
            normalized_endpoint: Some("wss://relay.example/".into()),
            admission,
            registration,
        }
    }

    fn route(
        role: AccountTransportRouteRoleFfi,
        state: AccountTransportRouteStateFfi,
        detail: RegistrationDetailCompletenessFfi,
        exact_count: Option<u32>,
    ) -> AccountTransportRouteStatusFfi {
        AccountTransportRouteStatusFfi {
            route_ref: "opaque".into(),
            group_id_hex: Some("abcd".into()),
            transport_group_id_hex: Some("ef".repeat(32)),
            role,
            state,
            requested_endpoint_count: 6,
            admitted_endpoint_count: 1,
            registered_endpoint_count: exact_count,
            registration_detail: detail,
            endpoints: vec![
                endpoint(
                    EndpointAdmissionOutcomeFfi::Allowed,
                    EndpointRegistrationOutcomeFfi::Registered,
                ),
                endpoint(
                    EndpointAdmissionOutcomeFfi::Invalid,
                    EndpointRegistrationOutcomeFfi::NotAttempted,
                ),
                endpoint(
                    EndpointAdmissionOutcomeFfi::Unsafe,
                    EndpointRegistrationOutcomeFfi::NotAttempted,
                ),
                endpoint(
                    EndpointAdmissionOutcomeFfi::Retired,
                    EndpointRegistrationOutcomeFfi::NotAttempted,
                ),
                endpoint(
                    EndpointAdmissionOutcomeFfi::Duplicate,
                    EndpointRegistrationOutcomeFfi::Pending,
                ),
                endpoint(
                    EndpointAdmissionOutcomeFfi::BeyondRouteLimit,
                    EndpointRegistrationOutcomeFfi::Unknown,
                ),
            ],
            pending_registration: true,
            pending_replay: true,
            retry_delay_ms: Some(60_000),
        }
    }

    #[test]
    fn transport_snapshot_deep_free_releases_every_nested_allocation() {
        let _guard = audit::test_lock();
        #[cfg(feature = "alloc-audit")]
        let before = audit::live_allocations();
        let routes = [
            (
                AccountTransportRouteRoleFfi::Inbox,
                AccountTransportRouteStateFfi::Pending,
            ),
            (
                AccountTransportRouteRoleFfi::CurrentGroup,
                AccountTransportRouteStateFfi::Registered,
            ),
            (
                AccountTransportRouteRoleFfi::CurrentGroup,
                AccountTransportRouteStateFfi::RetryPending,
            ),
            (
                AccountTransportRouteRoleFfi::HistoricalGroup,
                AccountTransportRouteStateFfi::PolicyBlocked,
            ),
        ]
        .into_iter()
        .map(|(role, state)| {
            route(
                role,
                state,
                RegistrationDetailCompletenessFfi::Exact,
                Some(1),
            )
        })
        .collect::<Vec<_>>();
        let mirror: MarmotAccountTransportStatusSnapshot = AccountTransportStatusSnapshotFfi {
            revision: 42,
            state: AccountTransportStateFfi::Degraded,
            inbox: Some(routes[0].clone()),
            current_group_routes: routes[1..3].to_vec(),
            historical_group_routes: vec![route(
                AccountTransportRouteRoleFfi::HistoricalGroup,
                AccountTransportRouteStateFfi::RetryPending,
                RegistrationDetailCompletenessFfi::Unknown,
                None,
            )],
        }
        .into();

        assert_eq!(mirror.revision, 42);
        assert!(!mirror.inbox.is_null());
        assert_eq!(mirror.current_group_routes_len, 2);
        assert_eq!(mirror.historical_group_routes_len, 1);
        unsafe {
            assert!(!(*mirror.historical_group_routes).has_registered_endpoint_count);
            assert!((*mirror.historical_group_routes).has_retry_delay_ms);
            marmot_account_transport_status_snapshot_free(boxed(mirror));
            marmot_account_transport_status_snapshot_free(std::ptr::null_mut());
        }
        #[cfg(feature = "alloc-audit")]
        assert_eq!(audit::live_allocations(), before);
    }
}
