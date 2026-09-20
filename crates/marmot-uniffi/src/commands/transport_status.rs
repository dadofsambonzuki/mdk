//! Read-only account transport coverage and its coalescing subscription.

use std::sync::Arc;

use crate::conversions::AccountTransportStatusSnapshotFfi;
use crate::subscriptions::AccountTransportStatusSubscription;
use crate::{Marmot, MarmotKitError};

#[uniffi::export]
impl Marmot {
    /// Read the latest process-local status without activating or dialing.
    pub fn account_transport_status(
        &self,
        account_ref: String,
    ) -> Result<AccountTransportStatusSnapshotFfi, MarmotKitError> {
        Ok(self.runtime.account_transport_status(&account_ref)?.into())
    }

    /// Attach to complete, size-one coalescing snapshots for one known account.
    pub fn subscribe_account_transport_status(
        &self,
        account_ref: String,
    ) -> Result<Arc<AccountTransportStatusSubscription>, MarmotKitError> {
        Ok(AccountTransportStatusSubscription::new(
            self.runtime
                .subscribe_account_transport_status(&account_ref)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversions::AccountTransportStateFfi;
    use marmot_account::AccountHome;
    use marmot_app::MarmotApp;
    use std::time::Duration;

    #[tokio::test]
    async fn inactive_query_and_subscription_do_not_activate_and_close_on_shutdown() {
        let root = tempfile::tempdir().expect("tempdir");
        AccountHome::open(root.path())
            .create_account("alice")
            .expect("create local account");
        let app = MarmotApp::with_relay(root.path(), "wss://relay.invalid.test");
        let runtime = app.runtime();
        let kit = Marmot { app, runtime };

        let queried = kit
            .account_transport_status("alice".into())
            .expect("known inactive account");
        assert_eq!(queried.revision, 0);
        assert_eq!(queried.state, AccountTransportStateFfi::Inactive);
        assert!(queried.inbox.is_none());

        let subscription = kit
            .subscribe_account_transport_status("alice".into())
            .expect("subscribe known inactive account");
        let initial = subscription.snapshot().expect("initial snapshot");
        assert_eq!(initial.state, AccountTransportStateFfi::Inactive);
        assert!(subscription.snapshot().is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), subscription.next())
                .await
                .is_err(),
            "an unchanged status must stay quiet"
        );

        kit.runtime.shutdown().await;
        assert!(subscription.next().await.is_none());
        assert!(kit.account_transport_status("unknown".into()).is_err());
    }
}
