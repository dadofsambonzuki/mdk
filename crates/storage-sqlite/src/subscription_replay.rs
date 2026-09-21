//! Durable replay floors for account transport subscriptions.
//!
//! This store is deliberately separate from NIP-77 reconciliation inventory.
//! A row means that one exact requested route incarnation still needs replay
//! coverage before its gap can be declared complete. Registration success does
//! not mutate these rows.

use crate::connection::CachedSql;
use crate::{SqliteAccountStorage, SqliteResultExt, i64_to_u64, u64_to_i64};
use cgka_traits::storage::{StorageError, StorageResult};
use cgka_traits::transport::Timestamp;
use cgka_traits::transport_adapter::TransportEndpoint;
use cgka_traits::types::GroupId;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const INBOX_ROUTE_KIND: i64 = 0;
const GROUP_ROUTE_KIND: i64 = 1;
const INBOX_ROUTE_ROLE: i64 = 0;
const CURRENT_GROUP_ROUTE_ROLE: i64 = 1;
const HISTORICAL_GROUP_ROUTE_ROLE: i64 = 2;
const GENERATION_BYTES: usize = 16;
const TRANSPORT_GROUP_ID_BYTES: usize = 32;

/// Whether a group route is the authenticated current route or retained
/// history. The role is explicit and part of durable identity; ordering never
/// promotes a historical route when the current route is policy-blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubscriptionReplayGroupRole {
    Current,
    Historical,
}

/// Exact requested route scope before local policy admission.
///
/// `normalized_endpoints` preserves first-seen source order. It includes
/// normalized policy-excluded endpoints as well as admitted endpoints, so a
/// later policy change cannot silently shrink the coverage obligation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionReplayRoute {
    Inbox {
        normalized_endpoints: Vec<TransportEndpoint>,
    },
    Group {
        group_id: GroupId,
        transport_group_id: Vec<u8>,
        role: SubscriptionReplayGroupRole,
        normalized_endpoints: Vec<TransportEndpoint>,
    },
}

impl SubscriptionReplayRoute {
    #[must_use]
    pub fn normalized_endpoints(&self) -> &[TransportEndpoint] {
        match self {
            Self::Inbox {
                normalized_endpoints,
            }
            | Self::Group {
                normalized_endpoints,
                ..
            } => normalized_endpoints,
        }
    }
}

/// Durable incarnation fence. A removed and re-added identical route receives
/// a new token, so stale EOSE and completion callbacks cannot clear it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionReplayGeneration([u8; GENERATION_BYTES]);

impl SubscriptionReplayGeneration {
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_bytes(bytes: [u8; GENERATION_BYTES]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; GENERATION_BYTES] {
        &self.0
    }
}

impl std::fmt::Debug for SubscriptionReplayGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SubscriptionReplayGeneration(<opaque>)")
    }
}

/// One outstanding replay gap. Row presence is the obligation; a `None`
/// floor means unfloored history and dominates every bounded floor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionReplayObligation {
    pub route: SubscriptionReplayRoute,
    pub replay_floor: Option<Timestamp>,
    pub generation: SubscriptionReplayGeneration,
    /// Canonical endpoint scope admitted by local policy during the latest
    /// preparation. An empty scope is a fully policy-blocked route.
    pub admitted_endpoints: Vec<TransportEndpoint>,
    /// Whether the admitted scope above reached EOSE and was durably
    /// checkpointed. The requested-scope obligation remains present when
    /// excluded coverage is still unresolved.
    pub admitted_scope_settled: bool,
}

impl SubscriptionReplayObligation {
    #[must_use]
    pub fn completion_fence(&self) -> SubscriptionReplayCompletionFence {
        SubscriptionReplayCompletionFence {
            generation: self.generation,
            replay_floor: self.replay_floor,
            admitted_scope_digest: endpoint_scope_digest(&self.admitted_endpoints),
        }
    }
}

/// Frozen replay-completion fence. The route incarnation, replay floor and
/// admitted endpoint scope must still match before EOSE may clear or settle
/// the obligation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriptionReplayCompletionFence {
    pub generation: SubscriptionReplayGeneration,
    pub replay_floor: Option<Timestamp>,
    admitted_scope_digest: [u8; 32],
}

/// One route to prepare before issuing its subscription. Preparing an
/// unchanged route preserves its generation and widens its floor only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionReplayPreparation {
    pub route: SubscriptionReplayRoute,
    pub replay_floor: Option<Timestamp>,
    pub admitted_endpoints: Vec<TransportEndpoint>,
    /// Explicit account-wide unfloored repair invalidates a prior admitted
    /// settlement. Historical routes can have a `None` floor without implying
    /// such a new repair request.
    pub reset_admitted_settlement: bool,
}

/// Per-route disposition after a replay prefix has been durably checkpointed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionReplayCompletion {
    /// Requested coverage is complete; remove the obligation.
    Clear(SubscriptionReplayCompletionFence),
    /// Only the currently admitted scope is complete; retain the requested
    /// coverage gap but stop widening unrelated routes until admission changes.
    SettleAdmittedScope(SubscriptionReplayCompletionFence),
}

impl SubscriptionReplayCompletion {
    fn fence(self) -> SubscriptionReplayCompletionFence {
        match self {
            Self::Clear(fence) | Self::SettleAdmittedScope(fence) => fence,
        }
    }
}

/// A replacement destination and the superseded generations whose unfinished
/// floors it must inherit. Every inherited generation must also appear in the
/// batch's retire list; the transaction prepares all destinations before it
/// retires any source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionReplayReplacement {
    pub preparation: SubscriptionReplayPreparation,
    pub inherit_from: Vec<SubscriptionReplayGeneration>,
}

/// Result of a replay-completion compare-and-clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionReplayClearResult {
    Cleared {
        count: usize,
    },
    /// At least one frozen generation was removed/replaced or its floor was
    /// widened after the snapshot. Nothing cleared.
    StaleGeneration,
    /// Delivery overflow remains durable. Nothing cleared.
    DeliveryOverflowPending,
}

/// Result of an atomic route-scoped completion batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionReplayCompletionResult {
    Completed { cleared: usize, settled: usize },
    StaleGeneration,
    DeliveryOverflowPending,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RouteKey {
    route_kind: i64,
    route_role: i64,
    group_id: Vec<u8>,
    transport_group_id: Vec<u8>,
    endpoint_scope_digest: [u8; 32],
}

#[derive(Clone)]
struct ValidatedRoute {
    key: RouteKey,
    route: SubscriptionReplayRoute,
}

struct RawObligation {
    key: RouteKey,
    replay_floor: Option<Timestamp>,
    generation: SubscriptionReplayGeneration,
    admitted_scope_settled: bool,
}

impl SqliteAccountStorage {
    /// Atomically prepare or widen every supplied route. Existing exact route
    /// identities retain their generation. The result follows input order.
    #[cfg(test)]
    pub(crate) fn prepare_subscription_replay_obligations(
        &self,
        preparations: &[SubscriptionReplayPreparation],
    ) -> StorageResult<Vec<SubscriptionReplayObligation>> {
        let replacements = preparations
            .iter()
            .cloned()
            .map(|preparation| SubscriptionReplayReplacement {
                preparation,
                inherit_from: Vec::new(),
            })
            .collect::<Vec<_>>();
        self.replace_subscription_replay_obligations(&replacements, &[])
    }

    /// Atomically prepare replacement destinations, conservatively transfer
    /// every inherited floor, then retire the exact superseded generations.
    ///
    /// `None` is unfloored and dominates. Missing/stale source generations or
    /// duplicate destinations reject the whole batch. Retire-only entries are
    /// authoritative removals; they are still generation-conditional so a
    /// stale process cannot remove an A -> B -> A re-addition.
    pub fn replace_subscription_replay_obligations(
        &self,
        replacements: &[SubscriptionReplayReplacement],
        retire: &[SubscriptionReplayGeneration],
    ) -> StorageResult<Vec<SubscriptionReplayObligation>> {
        let validated = validate_replacement_batch(replacements, retire)?;
        self.connection.with_transaction(|| {
            let mut inherited_floors = Vec::with_capacity(validated.len());
            for (replacement, _) in &validated {
                let mut floor = replacement.preparation.replay_floor;
                for source in &replacement.inherit_from {
                    let source_floor = self
                        .replay_floor_for_generation(source)?
                        .ok_or(StorageError::NotFound)?;
                    floor = merge_replay_floors(floor, source_floor);
                }
                inherited_floors.push(floor);
            }

            let mut prepared = Vec::with_capacity(validated.len());
            for ((replacement, route), floor) in validated.iter().zip(inherited_floors) {
                prepared.push(self.prepare_validated_subscription_replay(
                    route,
                    &replacement.preparation.admitted_endpoints,
                    floor,
                    replacement.preparation.reset_admitted_settlement,
                )?);
            }

            let prepared_generations = prepared
                .iter()
                .map(|obligation| obligation.generation)
                .collect::<HashSet<_>>();
            if retire
                .iter()
                .any(|generation| prepared_generations.contains(generation))
            {
                return Err(StorageError::Serialization(
                    "cannot retire the prepared subscription replay generation".to_owned(),
                ));
            }
            self.retire_subscription_replay_generations_tx(retire)?;
            Ok(prepared)
        })
    }

    /// Retire exact generations after authoritative route removal. The batch
    /// is all-or-nothing; a stale/missing generation rejects every retirement.
    #[cfg(test)]
    pub(crate) fn retire_subscription_replay_obligations(
        &self,
        generations: &[SubscriptionReplayGeneration],
    ) -> StorageResult<usize> {
        validate_unique_generations(generations, "retire")?;
        self.connection
            .with_transaction(|| self.retire_subscription_replay_generations_tx(generations))
    }

    /// Load every outstanding obligation in deterministic identity order.
    pub fn subscription_replay_obligations(
        &self,
    ) -> StorageResult<Vec<SubscriptionReplayObligation>> {
        let raw = {
            let conn = self.lock()?;
            let mut statement = conn
                .prepare_cached(
                    "SELECT route_kind, route_role, group_id, transport_group_id,
                            endpoint_scope_digest, replay_floor, generation,
                            admitted_scope_settled
                     FROM subscription_replay_obligations
                     ORDER BY route_kind, route_role, group_id,
                              transport_group_id, endpoint_scope_digest",
                )
                .storage()?;
            statement
                .query_map([], raw_obligation_from_row)
                .storage()?
                .collect::<Result<Vec<_>, _>>()
                .storage()?
        };
        raw.into_iter()
            .map(|obligation| self.hydrate_subscription_replay_obligation(obligation))
            .collect()
    }

    /// Clear a frozen completion set only if every generation and replay floor
    /// is still current and no account delivery-overflow marker is present.
    ///
    /// Call this in the same outer [`cgka_traits::storage::StorageProvider::with_transaction`]
    /// operation as the account projection checkpoint that precedes completion.
    /// SQLite's nested transaction support joins that caller-owned boundary.
    pub fn clear_subscription_replay_obligations(
        &self,
        account_label: &str,
        fences: &[SubscriptionReplayCompletionFence],
    ) -> StorageResult<SubscriptionReplayClearResult> {
        let completions = fences
            .iter()
            .copied()
            .map(SubscriptionReplayCompletion::Clear)
            .collect::<Vec<_>>();
        match self.complete_subscription_replay_obligations(account_label, &completions)? {
            SubscriptionReplayCompletionResult::Completed { cleared, .. } => {
                Ok(SubscriptionReplayClearResult::Cleared { count: cleared })
            }
            SubscriptionReplayCompletionResult::StaleGeneration => {
                Ok(SubscriptionReplayClearResult::StaleGeneration)
            }
            SubscriptionReplayCompletionResult::DeliveryOverflowPending => {
                Ok(SubscriptionReplayClearResult::DeliveryOverflowPending)
            }
        }
    }

    /// Atomically clear fully covered routes and settle only the admitted
    /// scope of routes whose requested coverage remains excluded or unknown.
    /// Every disposition is generation/floor fenced and overflow guarded.
    pub fn complete_subscription_replay_obligations(
        &self,
        account_label: &str,
        completions: &[SubscriptionReplayCompletion],
    ) -> StorageResult<SubscriptionReplayCompletionResult> {
        let generations = completions
            .iter()
            .map(|completion| completion.fence().generation)
            .collect::<Vec<_>>();
        validate_unique_generations(&generations, "clear")?;
        self.connection.with_transaction(|| {
            let conn = self.lock()?;
            let overflow_pending = conn
                .query_row_cached(
                    "SELECT EXISTS(
                        SELECT 1 FROM account_delivery_recovery WHERE account_label = ?1
                     )",
                    params![account_label],
                    |row| row.get::<_, bool>(0),
                )
                .storage()?;
            if overflow_pending {
                return Ok(SubscriptionReplayCompletionResult::DeliveryOverflowPending);
            }
            for completion in completions {
                let fence = completion.fence();
                let current_floor = conn
                    .query_row_cached(
                        "SELECT replay_floor FROM subscription_replay_obligations
                         WHERE generation = ?1",
                        params![fence.generation.as_bytes().as_slice()],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .optional()
                    .storage()?;
                let Some(current_floor) = current_floor else {
                    return Ok(SubscriptionReplayCompletionResult::StaleGeneration);
                };
                let current_floor = current_floor.map(i64_to_u64).transpose()?.map(Timestamp);
                if current_floor != fence.replay_floor {
                    return Ok(SubscriptionReplayCompletionResult::StaleGeneration);
                }
                let current_admitted = load_admitted_endpoints(&conn, &fence.generation)?;
                if endpoint_scope_digest(&current_admitted) != fence.admitted_scope_digest {
                    return Ok(SubscriptionReplayCompletionResult::StaleGeneration);
                }
            }
            let mut cleared = 0;
            let mut settled = 0;
            for completion in completions {
                let fence = completion.fence();
                match completion {
                    SubscriptionReplayCompletion::Clear(_) => {
                        conn.execute_cached(
                            "DELETE FROM subscription_replay_obligations WHERE generation = ?1",
                            params![fence.generation.as_bytes().as_slice()],
                        )
                        .storage()?;
                        cleared += 1;
                    }
                    SubscriptionReplayCompletion::SettleAdmittedScope(_) => {
                        conn.execute_cached(
                            "UPDATE subscription_replay_obligations
                             SET admitted_scope_settled = 1 WHERE generation = ?1",
                            params![fence.generation.as_bytes().as_slice()],
                        )
                        .storage()?;
                        settled += 1;
                    }
                }
            }
            Ok(SubscriptionReplayCompletionResult::Completed { cleared, settled })
        })
    }

    fn prepare_validated_subscription_replay(
        &self,
        route: &ValidatedRoute,
        admitted_endpoints: &[TransportEndpoint],
        replay_floor: Option<Timestamp>,
        reset_settlement: bool,
    ) -> StorageResult<SubscriptionReplayObligation> {
        let floor = replay_floor.map(|floor| u64_to_i64(floor.0)).transpose()?;
        let conn = self.lock()?;
        let key = &route.key;
        let previous_floor = conn
            .query_row_cached(
                "SELECT replay_floor FROM subscription_replay_obligations
                 WHERE route_kind = ?1 AND route_role = ?2 AND group_id = ?3
                   AND transport_group_id = ?4 AND endpoint_scope_digest = ?5",
                params![
                    key.route_kind,
                    key.route_role,
                    &key.group_id,
                    &key.transport_group_id,
                    key.endpoint_scope_digest.as_slice(),
                ],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .storage()?
            .map(|floor| {
                floor
                    .map(i64_to_u64)
                    .transpose()
                    .map(|floor| floor.map(Timestamp))
            })
            .transpose()?;
        conn.execute_cached(
            "INSERT INTO subscription_replay_obligations (
                route_kind, route_role, group_id, transport_group_id,
                endpoint_scope_digest, replay_floor, admitted_scope_settled
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
             ON CONFLICT(
                route_kind, route_role, group_id, transport_group_id, endpoint_scope_digest
             ) DO UPDATE SET replay_floor = CASE
                WHEN subscription_replay_obligations.replay_floor IS NULL
                  OR excluded.replay_floor IS NULL THEN NULL
                ELSE min(subscription_replay_obligations.replay_floor, excluded.replay_floor)
             END",
            params![
                key.route_kind,
                key.route_role,
                &key.group_id,
                &key.transport_group_id,
                key.endpoint_scope_digest.as_slice(),
                floor,
            ],
        )
        .storage()?;
        let raw = conn
            .query_row_cached(
                "SELECT route_kind, route_role, group_id, transport_group_id,
                        endpoint_scope_digest, replay_floor, generation,
                        admitted_scope_settled
                 FROM subscription_replay_obligations
                 WHERE route_kind = ?1 AND route_role = ?2 AND group_id = ?3
                   AND transport_group_id = ?4 AND endpoint_scope_digest = ?5",
                params![
                    key.route_kind,
                    key.route_role,
                    &key.group_id,
                    &key.transport_group_id,
                    key.endpoint_scope_digest.as_slice(),
                ],
                raw_obligation_from_row,
            )
            .storage()?;
        let existing_endpoints = load_endpoints(&conn, &raw.generation)?;
        if existing_endpoints.is_empty() && !route.route.normalized_endpoints().is_empty() {
            for (ordinal, endpoint) in route.route.normalized_endpoints().iter().enumerate() {
                conn.execute_cached(
                    "INSERT INTO subscription_replay_endpoints (generation, ordinal, endpoint)
                     VALUES (?1, ?2, ?3)",
                    params![
                        raw.generation.as_bytes().as_slice(),
                        i64::try_from(ordinal).map_err(|_| StorageError::Serialization(
                            "subscription replay endpoint ordinal exceeds SQLite range".to_owned()
                        ))?,
                        endpoint.as_str(),
                    ],
                )
                .storage()?;
            }
        } else if existing_endpoints != route.route.normalized_endpoints() {
            return Err(StorageError::Serialization(
                "subscription replay endpoint scope digest collision".to_owned(),
            ));
        }
        let existing_admitted = load_admitted_endpoints(&conn, &raw.generation)?;
        let settlement_invalidated = reset_settlement
            || existing_admitted != admitted_endpoints
            || previous_floor.is_some_and(|floor| floor != raw.replay_floor);
        if settlement_invalidated {
            conn.execute_cached(
                "UPDATE subscription_replay_obligations
                 SET admitted_scope_settled = 0 WHERE generation = ?1",
                params![raw.generation.as_bytes().as_slice()],
            )
            .storage()?;
        }
        replace_admitted_endpoints(&conn, &raw.generation, admitted_endpoints)?;
        drop(conn);
        let mut raw = raw;
        raw.admitted_scope_settled &= !settlement_invalidated;
        self.hydrate_subscription_replay_obligation(raw)
    }

    fn replay_floor_for_generation(
        &self,
        generation: &SubscriptionReplayGeneration,
    ) -> StorageResult<Option<Option<Timestamp>>> {
        let conn = self.lock()?;
        conn.query_row_cached(
            "SELECT replay_floor FROM subscription_replay_obligations WHERE generation = ?1",
            params![generation.as_bytes().as_slice()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .storage()?
        .map(|floor| {
            floor
                .map(i64_to_u64)
                .transpose()
                .map(|value| value.map(Timestamp))
        })
        .transpose()
    }

    fn retire_subscription_replay_generations_tx(
        &self,
        generations: &[SubscriptionReplayGeneration],
    ) -> StorageResult<usize> {
        validate_unique_generations(generations, "retire")?;
        let conn = self.lock()?;
        for generation in generations {
            let exists = conn
                .query_row_cached(
                    "SELECT EXISTS(
                        SELECT 1 FROM subscription_replay_obligations WHERE generation = ?1
                     )",
                    params![generation.as_bytes().as_slice()],
                    |row| row.get::<_, bool>(0),
                )
                .storage()?;
            if !exists {
                return Err(StorageError::NotFound);
            }
        }
        for generation in generations {
            conn.execute_cached(
                "DELETE FROM subscription_replay_obligations WHERE generation = ?1",
                params![generation.as_bytes().as_slice()],
            )
            .storage()?;
        }
        Ok(generations.len())
    }

    fn hydrate_subscription_replay_obligation(
        &self,
        raw: RawObligation,
    ) -> StorageResult<SubscriptionReplayObligation> {
        let conn = self.lock()?;
        let endpoints = load_endpoints(&conn, &raw.generation)?;
        let admitted_endpoints = load_admitted_endpoints(&conn, &raw.generation)?;
        let expected_digest = endpoint_scope_digest(&endpoints);
        if expected_digest != raw.key.endpoint_scope_digest {
            return Err(StorageError::Serialization(
                "subscription replay endpoint scope digest mismatch".to_owned(),
            ));
        }
        let route = match (raw.key.route_kind, raw.key.route_role) {
            (INBOX_ROUTE_KIND, INBOX_ROUTE_ROLE)
                if raw.key.group_id.is_empty() && raw.key.transport_group_id.is_empty() =>
            {
                SubscriptionReplayRoute::Inbox {
                    normalized_endpoints: endpoints,
                }
            }
            (GROUP_ROUTE_KIND, role)
                if !raw.key.group_id.is_empty()
                    && raw.key.transport_group_id.len() == TRANSPORT_GROUP_ID_BYTES =>
            {
                let role = match role {
                    CURRENT_GROUP_ROUTE_ROLE => SubscriptionReplayGroupRole::Current,
                    HISTORICAL_GROUP_ROUTE_ROLE => SubscriptionReplayGroupRole::Historical,
                    _ => {
                        return Err(StorageError::Serialization(
                            "invalid subscription replay group role".to_owned(),
                        ));
                    }
                };
                SubscriptionReplayRoute::Group {
                    group_id: GroupId::new(raw.key.group_id),
                    transport_group_id: raw.key.transport_group_id,
                    role,
                    normalized_endpoints: endpoints,
                }
            }
            _ => {
                return Err(StorageError::Serialization(
                    "invalid subscription replay route identity".to_owned(),
                ));
            }
        };
        Ok(SubscriptionReplayObligation {
            route,
            replay_floor: raw.replay_floor,
            generation: raw.generation,
            admitted_endpoints,
            admitted_scope_settled: raw.admitted_scope_settled,
        })
    }
}

fn validate_replacement_batch(
    replacements: &[SubscriptionReplayReplacement],
    retire: &[SubscriptionReplayGeneration],
) -> StorageResult<Vec<(SubscriptionReplayReplacement, ValidatedRoute)>> {
    validate_unique_generations(retire, "retire")?;
    let retired = retire.iter().copied().collect::<HashSet<_>>();
    let mut route_keys = HashSet::new();
    let mut validated = Vec::with_capacity(replacements.len());
    for replacement in replacements {
        validate_unique_generations(&replacement.inherit_from, "inherit")?;
        if replacement
            .inherit_from
            .iter()
            .any(|generation| !retired.contains(generation))
        {
            return Err(StorageError::Serialization(
                "inherited subscription replay generation must be retired in the same batch"
                    .to_owned(),
            ));
        }
        let route = validate_route(&replacement.preparation.route)?;
        validate_admitted_endpoints(
            &replacement.preparation.admitted_endpoints,
            route.route.normalized_endpoints(),
        )?;
        if !route_keys.insert(route.key.clone()) {
            return Err(StorageError::Serialization(
                "duplicate subscription replay destination".to_owned(),
            ));
        }
        validated.push((replacement.clone(), route));
    }
    Ok(validated)
}

fn validate_route(route: &SubscriptionReplayRoute) -> StorageResult<ValidatedRoute> {
    let endpoints = route.normalized_endpoints();
    let mut seen = HashSet::with_capacity(endpoints.len());
    for endpoint in endpoints {
        if endpoint.as_str().is_empty() || endpoint.as_str().len() > 4096 {
            return Err(StorageError::Serialization(
                "invalid normalized subscription replay endpoint".to_owned(),
            ));
        }
        if !seen.insert(endpoint.as_str()) {
            return Err(StorageError::Serialization(
                "duplicate normalized subscription replay endpoint".to_owned(),
            ));
        }
    }
    let endpoint_scope_digest = endpoint_scope_digest(endpoints);
    let key = match route {
        SubscriptionReplayRoute::Inbox { .. } => RouteKey {
            route_kind: INBOX_ROUTE_KIND,
            route_role: INBOX_ROUTE_ROLE,
            group_id: Vec::new(),
            transport_group_id: Vec::new(),
            endpoint_scope_digest,
        },
        SubscriptionReplayRoute::Group {
            group_id,
            transport_group_id,
            role,
            ..
        } => {
            if group_id.as_slice().is_empty() {
                return Err(StorageError::Serialization(
                    "subscription replay MLS group id is empty".to_owned(),
                ));
            }
            if transport_group_id.len() != TRANSPORT_GROUP_ID_BYTES {
                return Err(StorageError::Serialization(format!(
                    "subscription replay transport group id must be {TRANSPORT_GROUP_ID_BYTES} bytes"
                )));
            }
            RouteKey {
                route_kind: GROUP_ROUTE_KIND,
                route_role: match role {
                    SubscriptionReplayGroupRole::Current => CURRENT_GROUP_ROUTE_ROLE,
                    SubscriptionReplayGroupRole::Historical => HISTORICAL_GROUP_ROUTE_ROLE,
                },
                group_id: group_id.as_slice().to_vec(),
                transport_group_id: transport_group_id.clone(),
                endpoint_scope_digest,
            }
        }
    };
    Ok(ValidatedRoute {
        key,
        route: route.clone(),
    })
}

fn validate_admitted_endpoints(
    admitted: &[TransportEndpoint],
    requested: &[TransportEndpoint],
) -> StorageResult<()> {
    let requested = requested
        .iter()
        .map(TransportEndpoint::as_str)
        .collect::<HashSet<_>>();
    let mut seen = HashSet::with_capacity(admitted.len());
    for endpoint in admitted {
        if endpoint.as_str().is_empty()
            || endpoint.as_str().len() > 4096
            || !seen.insert(endpoint.as_str())
            || !requested.contains(endpoint.as_str())
        {
            return Err(StorageError::Serialization(
                "invalid admitted subscription replay endpoint scope".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_unique_generations(
    generations: &[SubscriptionReplayGeneration],
    operation: &str,
) -> StorageResult<()> {
    let mut unique = HashSet::with_capacity(generations.len());
    if generations
        .iter()
        .any(|generation| !unique.insert(*generation))
    {
        return Err(StorageError::Serialization(format!(
            "duplicate subscription replay generation in {operation} batch"
        )));
    }
    Ok(())
}

fn endpoint_scope_digest(endpoints: &[TransportEndpoint]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((endpoints.len() as u64).to_be_bytes());
    for endpoint in endpoints {
        let bytes = endpoint.as_str().as_bytes();
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    hasher.finalize().into()
}

fn merge_replay_floors(left: Option<Timestamp>, right: Option<Timestamp>) -> Option<Timestamp> {
    match (left, right) {
        (None, _) | (_, None) => None,
        (Some(left), Some(right)) => Some(Timestamp(left.0.min(right.0))),
    }
}

fn raw_obligation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawObligation> {
    let endpoint_scope_digest = fixed_blob::<32>(row.get(4)?, 4)?;
    let generation = SubscriptionReplayGeneration(fixed_blob::<GENERATION_BYTES>(row.get(6)?, 6)?);
    let replay_floor = row
        .get::<_, Option<i64>>(5)?
        .map(|value| {
            u64::try_from(value)
                .map(Timestamp)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(5, value))
        })
        .transpose()?;
    Ok(RawObligation {
        key: RouteKey {
            route_kind: row.get(0)?,
            route_role: row.get(1)?,
            group_id: row.get(2)?,
            transport_group_id: row.get(3)?,
            endpoint_scope_digest,
        },
        replay_floor,
        generation,
        admitted_scope_settled: row.get(7)?,
    })
}

fn fixed_blob<const N: usize>(bytes: Vec<u8>, column: usize) -> rusqlite::Result<[u8; N]> {
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Blob,
            format!("expected {N} bytes, found {}", bytes.len()).into(),
        )
    })
}

fn load_endpoints(
    conn: &rusqlite::Connection,
    generation: &SubscriptionReplayGeneration,
) -> StorageResult<Vec<TransportEndpoint>> {
    let mut statement = conn
        .prepare_cached(
            "SELECT endpoint FROM subscription_replay_endpoints
             WHERE generation = ?1 ORDER BY ordinal",
        )
        .storage()?;
    statement
        .query_map(params![generation.as_bytes().as_slice()], |row| {
            row.get::<_, String>(0).map(TransportEndpoint)
        })
        .storage()?
        .collect::<Result<Vec<_>, _>>()
        .storage()
}

fn load_admitted_endpoints(
    conn: &rusqlite::Connection,
    generation: &SubscriptionReplayGeneration,
) -> StorageResult<Vec<TransportEndpoint>> {
    let mut statement = conn
        .prepare_cached(
            "SELECT endpoint FROM subscription_replay_admitted_endpoints
             WHERE generation = ?1 ORDER BY ordinal",
        )
        .storage()?;
    statement
        .query_map(params![generation.as_bytes().as_slice()], |row| {
            row.get::<_, String>(0).map(TransportEndpoint)
        })
        .storage()?
        .collect::<Result<Vec<_>, _>>()
        .storage()
}

fn replace_admitted_endpoints(
    conn: &rusqlite::Connection,
    generation: &SubscriptionReplayGeneration,
    endpoints: &[TransportEndpoint],
) -> StorageResult<()> {
    conn.execute_cached(
        "DELETE FROM subscription_replay_admitted_endpoints WHERE generation = ?1",
        params![generation.as_bytes().as_slice()],
    )
    .storage()?;
    for (ordinal, endpoint) in endpoints.iter().enumerate() {
        conn.execute_cached(
            "INSERT INTO subscription_replay_admitted_endpoints (generation, ordinal, endpoint)
             VALUES (?1, ?2, ?3)",
            params![
                generation.as_bytes().as_slice(),
                i64::try_from(ordinal).map_err(|_| StorageError::Serialization(
                    "subscription replay admitted endpoint ordinal exceeds SQLite range".to_owned()
                ))?,
                endpoint.as_str(),
            ],
        )
        .storage()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
