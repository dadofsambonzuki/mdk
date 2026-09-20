//! C handle for one account's bounded, coalescing transport status.

use super::*;
use crate::types::transport_status::MarmotAccountTransportStatusSnapshot;
use marmot_uniffi::AccountTransportStatusSubscription;

// The mirror's raw pointers exclusively own the allocations they reference.
unsafe impl Send for MarmotAccountTransportStatusSnapshot {}

c_subscription! {
    /// One account's complete transport coverage. Free before its client.
    MarmotAccountTransportStatusSubscription(AccountTransportStatusSubscription),
    item MarmotAccountTransportStatusSnapshot from marmot_uniffi::conversions::AccountTransportStatusSnapshotFfi,
    item_free "marmot_account_transport_status_snapshot_free",
    callback MarmotAccountTransportStatusCallback,
    read next,
    next marmot_account_transport_status_subscription_next,
    set_callback marmot_account_transport_status_subscription_set_callback,
    clear_callback marmot_account_transport_status_subscription_clear_callback,
    free marmot_account_transport_status_subscription_free
}

/// Subscribe without activating or dialing. Take the initial snapshot once, then
/// drive `next` or install a callback; do not mix the two receive modes.
///
/// # Safety
/// `client` and `account_ref` must be valid; `out_sub` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn marmot_subscribe_account_transport_status(
    client: *const MarmotClient,
    account_ref: *const c_char,
    out_sub: *mut *mut MarmotAccountTransportStatusSubscription,
) -> MarmotStatus {
    ffi_guard(|| {
        try_arg!(unsafe { preflight_out_ptr(out_sub) });
        let client = try_arg!(unsafe { client_ref(client) });
        let account_ref = try_arg!(unsafe { required_str(account_ref) });
        match client
            .marmot
            .subscribe_account_transport_status(account_ref)
        {
            Ok(inner) => unsafe {
                write_handle(
                    MarmotAccountTransportStatusSubscription {
                        core: SubscriptionCore::new(client.runtime.handle().clone()),
                        inner,
                    },
                    out_sub,
                )
            },
            Err(error) => status_from_error(&error),
        }
    })
}

/// Take the initial complete snapshot once. A second call returns CLOSED and NULL.
/// Free the result with `marmot_account_transport_status_snapshot_free`.
///
/// # Safety
/// `sub` must remain live and `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn marmot_account_transport_status_subscription_snapshot(
    sub: *const MarmotAccountTransportStatusSubscription,
    out: *mut *mut MarmotAccountTransportStatusSnapshot,
) -> MarmotStatus {
    ffi_guard(|| {
        try_arg!(unsafe { preflight_out_ptr(out) });
        let sub = try_arg!(unsafe { sub_ref(sub) });
        unsafe { deliver_next(Ok(sub.inner.snapshot()), out) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::transport_status::marmot_account_transport_status_snapshot_free;
    use marmot_uniffi::{Marmot, MarmotKitError, SecretStore};
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::ptr;

    #[derive(Default)]
    struct Store(StdMutex<HashMap<String, String>>);

    impl SecretStore for Store {
        fn has_secret_for_label(&self, label: String) -> Result<bool, MarmotKitError> {
            Ok(self.0.lock().unwrap().contains_key(&label))
        }

        fn has_secret_for_account_id(&self, _: String) -> Result<bool, MarmotKitError> {
            Ok(false)
        }

        fn write_secret(
            &self,
            label: String,
            _: String,
            secret: String,
        ) -> Result<(), MarmotKitError> {
            self.0.lock().unwrap().insert(label, secret);
            Ok(())
        }

        fn load_secret(&self, label: String, _: String) -> Result<String, MarmotKitError> {
            self.0
                .lock()
                .unwrap()
                .get(&label)
                .cloned()
                .ok_or(MarmotKitError::SecretNotFound {
                    details: "test".into(),
                })
        }

        fn remove_secret(&self, label: String, _: String) -> Result<(), MarmotKitError> {
            self.0.lock().unwrap().remove(&label);
            Ok(())
        }
    }

    #[test]
    fn c_transport_status_query_snapshot_ownership_and_shutdown() {
        let _guard = crate::memory::audit::test_lock();
        #[cfg(feature = "alloc-audit")]
        let before = crate::memory::audit::live_allocations();
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let relay = runtime
            .block_on(nostr_relay_builder::MockRelay::run())
            .unwrap();
        let url = runtime.block_on(relay.url()).to_string();
        let marmot = Marmot::new_with_options(
            dir.path().to_str().unwrap().into(),
            vec![url.clone()],
            marmot_uniffi::RelayPolicyFfi::AllowLoopback,
            Some(Arc::new(Store::default())),
        )
        .unwrap();
        let client = MarmotClient { runtime, marmot };
        let account = client
            .block_on(client.marmot.create_identity(vec![url.clone()], vec![url]))
            .unwrap()
            .account_id_hex;
        let account = CString::new(account).unwrap();

        unsafe {
            assert_eq!(
                crate::commands::marmot_account_transport_status(
                    &client,
                    account.as_ptr(),
                    ptr::null_mut(),
                ),
                MarmotStatus::NullPointer,
            );
            let mut queried = ptr::null_mut();
            assert_eq!(
                crate::commands::marmot_account_transport_status(
                    &client,
                    account.as_ptr(),
                    &mut queried,
                ),
                MarmotStatus::Ok,
            );
            assert!(!queried.is_null());
            marmot_account_transport_status_snapshot_free(queried);

            let mut subscription = ptr::null_mut();
            assert_eq!(
                marmot_subscribe_account_transport_status(
                    &client,
                    account.as_ptr(),
                    &mut subscription,
                ),
                MarmotStatus::Ok,
            );
            assert_eq!(
                marmot_account_transport_status_subscription_snapshot(
                    subscription,
                    ptr::null_mut(),
                ),
                MarmotStatus::NullPointer,
            );
            let mut initial = ptr::null_mut();
            assert_eq!(
                marmot_account_transport_status_subscription_snapshot(subscription, &mut initial,),
                MarmotStatus::Ok,
            );
            assert!(!initial.is_null());
            marmot_account_transport_status_snapshot_free(initial);
            assert_eq!(
                marmot_account_transport_status_subscription_snapshot(subscription, &mut initial,),
                MarmotStatus::Closed,
            );
            assert!(initial.is_null());

            client.block_on(client.marmot.shutdown_and_close()).unwrap();
            assert_eq!(
                marmot_account_transport_status_subscription_next(subscription, 100, &mut initial,),
                MarmotStatus::Closed,
            );
            assert!(initial.is_null());
            marmot_account_transport_status_subscription_free(subscription);
            marmot_account_transport_status_subscription_free(ptr::null_mut());
        }

        drop(relay);
        drop(client);
        let _ = crate::status::take_last_error();
        #[cfg(feature = "alloc-audit")]
        assert_eq!(crate::memory::audit::live_allocations(), before);
    }
}
