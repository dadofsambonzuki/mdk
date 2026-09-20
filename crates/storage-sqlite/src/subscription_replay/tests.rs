use super::*;
use crate::{SqlCipherKey, StoredAccountState};
use cgka_traits::storage::{StorageError, StorageProvider, StorageResult};

fn endpoints(values: &[&str]) -> Vec<TransportEndpoint> {
    values
        .iter()
        .map(|value| TransportEndpoint::from(*value))
        .collect()
}

fn inbox(values: &[&str]) -> SubscriptionReplayRoute {
    SubscriptionReplayRoute::Inbox {
        normalized_endpoints: endpoints(values),
    }
}

fn group(
    group_byte: u8,
    route_byte: u8,
    role: SubscriptionReplayGroupRole,
    values: &[&str],
) -> SubscriptionReplayRoute {
    SubscriptionReplayRoute::Group {
        // Deliberately not 32 bytes: MLS group ids are opaque and variable-length.
        group_id: GroupId::new(vec![group_byte; 7]),
        transport_group_id: vec![route_byte; 32],
        role,
        normalized_endpoints: endpoints(values),
    }
}

fn prepare(
    route: SubscriptionReplayRoute,
    replay_floor: Option<u64>,
) -> SubscriptionReplayPreparation {
    SubscriptionReplayPreparation {
        route,
        replay_floor: replay_floor.map(Timestamp),
    }
}

fn completion_fence(
    obligation: &SubscriptionReplayObligation,
) -> SubscriptionReplayCompletionFence {
    SubscriptionReplayCompletionFence {
        generation: obligation.generation,
        replay_floor: obligation.replay_floor,
    }
}

#[test]
fn prepare_is_idempotent_widens_earliest_floor_and_unfloored_dominates() {
    let store = SqliteAccountStorage::in_memory().unwrap();
    let route = inbox(&["wss://one.example", "wss://two.example"]);

    let first = store
        .prepare_subscription_replay_obligations(&[prepare(route.clone(), Some(100))])
        .unwrap()
        .remove(0);
    let later = store
        .prepare_subscription_replay_obligations(&[prepare(route.clone(), Some(200))])
        .unwrap()
        .remove(0);
    assert_eq!(later.generation, first.generation);
    assert_eq!(later.replay_floor, Some(Timestamp(100)));

    let earlier = store
        .prepare_subscription_replay_obligations(&[prepare(route.clone(), Some(50))])
        .unwrap()
        .remove(0);
    assert_eq!(earlier.generation, first.generation);
    assert_eq!(earlier.replay_floor, Some(Timestamp(50)));

    let unfloored = store
        .prepare_subscription_replay_obligations(&[prepare(route.clone(), None)])
        .unwrap()
        .remove(0);
    assert_eq!(unfloored.generation, first.generation);
    assert_eq!(unfloored.replay_floor, None);

    let bounded_again = store
        .prepare_subscription_replay_obligations(&[prepare(route, Some(1))])
        .unwrap()
        .remove(0);
    assert_eq!(bounded_again.generation, first.generation);
    assert_eq!(bounded_again.replay_floor, None);
    assert_eq!(store.subscription_replay_obligations().unwrap().len(), 1);
}

#[test]
fn inbox_current_and_historical_routes_have_distinct_durable_identity() {
    let store = SqliteAccountStorage::in_memory().unwrap();
    let scope = ["wss://relay.example"];
    let current = group(1, 2, SubscriptionReplayGroupRole::Current, &scope);
    let historical = group(1, 2, SubscriptionReplayGroupRole::Historical, &scope);
    let prepared = store
        .prepare_subscription_replay_obligations(&[
            prepare(inbox(&scope), Some(10)),
            prepare(current.clone(), Some(20)),
            prepare(historical.clone(), None),
        ])
        .unwrap();

    assert_eq!(prepared.len(), 3);
    assert_ne!(prepared[0].generation, prepared[1].generation);
    assert_ne!(prepared[1].generation, prepared[2].generation);
    assert_eq!(prepared[1].route, current);
    assert_eq!(prepared[1].replay_floor, Some(Timestamp(20)));
    assert_eq!(prepared[2].route, historical);
    assert_eq!(prepared[2].replay_floor, None);
}

#[test]
fn obligations_and_original_endpoint_order_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("subscription-replay.sqlite3");
    let key = SqlCipherKey::new("subscription replay persistence key").unwrap();
    let route = group(
        3,
        4,
        SubscriptionReplayGroupRole::Current,
        &["wss://z.example", "wss://a.example"],
    );
    let expected = {
        let store = SqliteAccountStorage::open_encrypted(&path, &key).unwrap();
        store
            .prepare_subscription_replay_obligations(&[prepare(route, Some(42))])
            .unwrap()
            .remove(0)
    };

    let reopened = SqliteAccountStorage::open_encrypted(&path, &key).unwrap();
    assert_eq!(
        reopened.subscription_replay_obligations().unwrap(),
        [expected]
    );
}

#[test]
fn replacement_transfers_floor_before_retiring_and_readd_gets_new_generation() {
    let store = SqliteAccountStorage::in_memory().unwrap();
    let route_a = group(
        5,
        6,
        SubscriptionReplayGroupRole::Current,
        &["wss://a.example"],
    );
    let route_b = group(
        5,
        7,
        SubscriptionReplayGroupRole::Current,
        &["wss://b.example"],
    );
    let first_a = store
        .prepare_subscription_replay_obligations(&[prepare(route_a.clone(), Some(100))])
        .unwrap()
        .remove(0);

    let replacement_b = store
        .replace_subscription_replay_obligations(
            &[SubscriptionReplayReplacement {
                preparation: prepare(route_b, Some(200)),
                inherit_from: vec![first_a.generation],
            }],
            &[first_a.generation],
        )
        .unwrap()
        .remove(0);
    assert_eq!(replacement_b.replay_floor, Some(Timestamp(100)));
    assert_ne!(replacement_b.generation, first_a.generation);
    let listed = store.subscription_replay_obligations().unwrap();
    assert_eq!(listed.as_slice(), std::slice::from_ref(&replacement_b));

    assert_eq!(
        store
            .retire_subscription_replay_obligations(&[replacement_b.generation])
            .unwrap(),
        1
    );
    let second_a = store
        .prepare_subscription_replay_obligations(&[prepare(route_a, Some(300))])
        .unwrap()
        .remove(0);
    assert_ne!(second_a.generation, first_a.generation);
}

#[test]
fn failed_replacement_is_atomic_and_preserves_source() {
    let store = SqliteAccountStorage::in_memory().unwrap();
    let source = store
        .prepare_subscription_replay_obligations(&[prepare(
            inbox(&["wss://source.example"]),
            Some(10),
        )])
        .unwrap()
        .remove(0);
    let missing = SubscriptionReplayGeneration::from_bytes([0xee; 16]);

    let result = store.replace_subscription_replay_obligations(
        &[SubscriptionReplayReplacement {
            preparation: prepare(inbox(&["wss://replacement.example"]), Some(20)),
            inherit_from: vec![missing],
        }],
        &[missing],
    );
    assert!(matches!(result, Err(StorageError::NotFound)));
    assert_eq!(store.subscription_replay_obligations().unwrap(), [source]);
}

#[test]
fn clear_is_generation_conditional_and_fenced_by_delivery_overflow() {
    let store = SqliteAccountStorage::in_memory().unwrap();
    store.ensure_account_projection("alice").unwrap();
    let obligation = store
        .prepare_subscription_replay_obligations(&[prepare(
            inbox(&["wss://relay.example"]),
            Some(10),
        )])
        .unwrap()
        .remove(0);

    store.mark_account_delivery_recovery("alice", 7, 1).unwrap();
    assert_eq!(
        store
            .clear_subscription_replay_obligations("alice", &[completion_fence(&obligation)])
            .unwrap(),
        SubscriptionReplayClearResult::DeliveryOverflowPending
    );
    assert_eq!(store.subscription_replay_obligations().unwrap().len(), 1);
    assert!(store.clear_account_delivery_recovery("alice", 7).unwrap());

    assert_eq!(
        store
            .clear_subscription_replay_obligations(
                "alice",
                &[
                    completion_fence(&obligation),
                    SubscriptionReplayCompletionFence {
                        generation: SubscriptionReplayGeneration::from_bytes([0xaa; 16]),
                        replay_floor: Some(Timestamp(10)),
                    },
                ],
            )
            .unwrap(),
        SubscriptionReplayClearResult::StaleGeneration
    );
    assert_eq!(store.subscription_replay_obligations().unwrap().len(), 1);
    assert_eq!(
        store
            .clear_subscription_replay_obligations("alice", &[completion_fence(&obligation)])
            .unwrap(),
        SubscriptionReplayClearResult::Cleared { count: 1 }
    );
    assert!(store.subscription_replay_obligations().unwrap().is_empty());
}

#[test]
fn widened_floor_invalidates_a_frozen_completion_fence() {
    let store = SqliteAccountStorage::in_memory().unwrap();
    store.ensure_account_projection("alice").unwrap();
    let route = inbox(&["wss://relay.example"]);
    let frozen = store
        .prepare_subscription_replay_obligations(&[prepare(route.clone(), Some(100))])
        .unwrap()
        .remove(0);
    let widened = store
        .prepare_subscription_replay_obligations(&[prepare(route, Some(50))])
        .unwrap()
        .remove(0);
    assert_eq!(widened.generation, frozen.generation);
    assert_eq!(widened.replay_floor, Some(Timestamp(50)));

    assert_eq!(
        store
            .clear_subscription_replay_obligations("alice", &[completion_fence(&frozen)])
            .unwrap(),
        SubscriptionReplayClearResult::StaleGeneration
    );
    assert_eq!(store.subscription_replay_obligations().unwrap(), [widened]);
}

#[test]
fn replay_clear_and_projection_checkpoint_share_one_crash_boundary() {
    let store = SqliteAccountStorage::in_memory().unwrap();
    let initial = StoredAccountState {
        label: "alice".to_owned(),
        last_transport_timestamp: Some(1),
        ..StoredAccountState::default()
    };
    store
        .save_account_projection_state(&initial, 32, 120)
        .unwrap();
    let obligation = store
        .prepare_subscription_replay_obligations(&[prepare(
            inbox(&["wss://relay.example"]),
            Some(1),
        )])
        .unwrap()
        .remove(0);
    let advanced = StoredAccountState {
        last_transport_timestamp: Some(20),
        ..initial.clone()
    };

    let rolled_back: StorageResult<()> = store.with_transaction(|store| {
        store.save_account_projection_state(&advanced, 32, 120)?;
        assert_eq!(
            store
                .clear_subscription_replay_obligations("alice", &[completion_fence(&obligation)],)?,
            SubscriptionReplayClearResult::Cleared { count: 1 }
        );
        Err(StorageError::Backend("injected crash boundary".to_owned()))
    });
    assert!(rolled_back.is_err());
    assert_eq!(
        store
            .load_account_projection_state("alice", 32)
            .unwrap()
            .last_transport_timestamp,
        Some(1)
    );
    assert_eq!(store.subscription_replay_obligations().unwrap().len(), 1);

    store
        .with_transaction::<_, StorageError, _>(|store| {
            store.save_account_projection_state(&advanced, 32, 120)?;
            assert_eq!(
                store.clear_subscription_replay_obligations(
                    "alice",
                    &[completion_fence(&obligation)],
                )?,
                SubscriptionReplayClearResult::Cleared { count: 1 }
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(
        store
            .load_account_projection_state("alice", 32)
            .unwrap()
            .last_transport_timestamp,
        Some(20)
    );
    assert!(store.subscription_replay_obligations().unwrap().is_empty());
}

#[test]
fn duplicate_or_malformed_batches_fail_before_mutation() {
    let store = SqliteAccountStorage::in_memory().unwrap();
    let route = inbox(&["wss://relay.example"]);
    let duplicate = store.prepare_subscription_replay_obligations(&[
        prepare(route.clone(), Some(1)),
        prepare(route, Some(2)),
    ]);
    assert!(matches!(duplicate, Err(StorageError::Serialization(_))));

    let malformed = store.prepare_subscription_replay_obligations(&[prepare(
        SubscriptionReplayRoute::Group {
            group_id: GroupId::new(vec![1]),
            transport_group_id: vec![2; 31],
            role: SubscriptionReplayGroupRole::Current,
            normalized_endpoints: endpoints(&["wss://relay.example"]),
        },
        Some(1),
    )]);
    assert!(matches!(malformed, Err(StorageError::Serialization(_))));
    assert!(store.subscription_replay_obligations().unwrap().is_empty());
}
