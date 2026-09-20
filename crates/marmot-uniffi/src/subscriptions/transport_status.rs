//! One account's complete coalescing transport-status snapshots.

use super::{Mutex, StdMutex, take_snapshot};
use crate::conversions::AccountTransportStatusSnapshotFfi;
use std::sync::Arc;

#[derive(uniffi::Object)]
pub struct AccountTransportStatusSubscription {
    snapshot: StdMutex<Option<AccountTransportStatusSnapshotFfi>>,
    receiver: Mutex<marmot_app::RuntimeAccountTransportStatusSubscription>,
}

impl AccountTransportStatusSubscription {
    pub(crate) fn new(inner: marmot_app::RuntimeAccountTransportStatusSubscription) -> Arc<Self> {
        let snapshot = inner.snapshot.clone().into();
        Arc::new(Self {
            snapshot: StdMutex::new(Some(snapshot)),
            receiver: Mutex::new(inner),
        })
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl AccountTransportStatusSubscription {
    /// Take the initial complete snapshot once, before driving `next`.
    pub fn snapshot(&self) -> Option<AccountTransportStatusSnapshotFfi> {
        take_snapshot(&self.snapshot)
    }

    /// Await the newest semantic replacement. Intermediate revisions may coalesce.
    pub async fn next(&self) -> Option<AccountTransportStatusSnapshotFfi> {
        self.receiver.lock().await.recv().await.map(Into::into)
    }
}
