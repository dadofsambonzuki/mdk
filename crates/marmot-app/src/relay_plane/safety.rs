use std::net::IpAddr;

use cgka_traits::app_components::{is_loopback_host, reject_non_public_ip};
use cgka_traits::{TransportEndpoint, TransportPublishRequest, TransportPublishTarget};
use nostr_sdk::prelude::RelayUrl;
use serde::{Deserialize, Serialize};
use url::{Host, Url};

const MAX_RELAY_ENDPOINTS_PER_ROUTE: usize = 16;
const RETIRED_RELAY_HOSTS: &[&str] = &["relay.nostr.band"];

/// The policy decision for one caller-supplied Nostr relay endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayEndpointPolicy {
    Allowed,
    Retired,
    Invalid,
    Unsafe,
}

/// One endpoint's normalized relay URL and policy decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayEndpointClassification {
    /// The caller-supplied value, preserved so batch callers can associate the
    /// decision with their input without relying on positional matching.
    pub endpoint: String,
    /// Canonical relay URL when parsing succeeded, including for retired or
    /// unsafe endpoints. Invalid inputs have no normalized representation.
    pub normalized_endpoint: Option<String>,
    pub policy: RelayEndpointPolicy,
}

/// Subscription-only disposition for one requested endpoint.
///
/// Unlike [`RelayEndpointPolicy`], this also records route-level decisions
/// such as deduplication and the bounded endpoint cap. It is kept separate so
/// configuration and publish validation remain strict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RelaySubscriptionEndpointDisposition {
    Admitted,
    Invalid,
    Unsafe,
    Retired,
    Duplicate,
    BeyondRouteLimit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelaySubscriptionEndpointOutcome {
    pub requested_endpoint: String,
    pub normalized_endpoint: Option<String>,
    pub disposition: RelaySubscriptionEndpointDisposition,
}

/// Local admission result for one inbox or group subscription route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelaySubscriptionAdmission {
    pub requested_endpoints: Vec<TransportEndpoint>,
    pub admitted_endpoints: Vec<TransportEndpoint>,
    pub endpoint_outcomes: Vec<RelaySubscriptionEndpointOutcome>,
}

/// Hostnames that the relay plane will never dial or adopt.
pub fn retired_relay_hosts() -> Vec<String> {
    RETIRED_RELAY_HOSTS
        .iter()
        .map(|host| (*host).to_owned())
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct RelaySafetyPolicy {
    max_endpoints_per_route: usize,
    /// Dev/test opt-in for loopback relay endpoints
    /// (`MarmotAppConfig::allow_loopback_relay_endpoints`). Off by default:
    /// production rejects relay endpoints whose LITERAL host is loopback (or
    /// any other non-public IP literal) before the URL reaches the relay pool.
    allow_loopback: bool,
}

impl Default for RelaySafetyPolicy {
    fn default() -> Self {
        Self {
            max_endpoints_per_route: MAX_RELAY_ENDPOINTS_PER_ROUTE,
            allow_loopback: false,
        }
    }
}

impl RelaySafetyPolicy {
    pub(crate) fn with_allow_loopback(allow_loopback: bool) -> Self {
        Self {
            allow_loopback,
            ..Self::default()
        }
    }

    pub(crate) fn classify_endpoints(
        &self,
        endpoints: Vec<String>,
    ) -> Vec<RelayEndpointClassification> {
        endpoints
            .into_iter()
            .map(|endpoint| self.classify_endpoint(endpoint))
            .collect()
    }

    fn classify_endpoint(&self, endpoint: String) -> RelayEndpointClassification {
        let raw = endpoint.trim();
        let Ok(relay_url) = RelayUrl::parse(raw) else {
            return RelayEndpointClassification {
                endpoint,
                normalized_endpoint: None,
                policy: RelayEndpointPolicy::Invalid,
            };
        };
        let normalized_endpoint = Some(relay_url.to_string());
        let policy = match evaluate_relay_url(&relay_url, self.allow_loopback) {
            Ok(()) => RelayEndpointPolicy::Allowed,
            Err(rejection) => rejection.policy(),
        };
        RelayEndpointClassification {
            endpoint,
            normalized_endpoint,
            policy,
        }
    }

    /// Admit subscription endpoints independently in source order.
    ///
    /// This is intentionally narrower than [`Self::sanitize_endpoints`]:
    /// signed/discovered subscription routes are untrusted input, so one bad
    /// endpoint degrades that route rather than rejecting every route in the
    /// account. Configuration setters and publish validation continue to use
    /// the strict all-or-nothing path.
    pub(crate) fn admit_subscription_endpoints(
        &self,
        endpoints: Vec<TransportEndpoint>,
    ) -> RelaySubscriptionAdmission {
        let requested_endpoints = endpoints.clone();
        let mut admitted_endpoints =
            Vec::with_capacity(endpoints.len().min(self.max_endpoints_per_route));
        let mut endpoint_outcomes = Vec::with_capacity(endpoints.len());

        for endpoint in endpoints {
            let requested_endpoint = endpoint.0;
            let raw = requested_endpoint.trim();
            let Ok(relay_url) = RelayUrl::parse(raw) else {
                endpoint_outcomes.push(RelaySubscriptionEndpointOutcome {
                    requested_endpoint,
                    normalized_endpoint: None,
                    disposition: RelaySubscriptionEndpointDisposition::Invalid,
                });
                continue;
            };
            let normalized_endpoint = relay_url.to_string();
            let disposition = match evaluate_relay_url(&relay_url, self.allow_loopback) {
                Err(RelayEndpointRejection::Retired) => {
                    RelaySubscriptionEndpointDisposition::Retired
                }
                Err(RelayEndpointRejection::Invalid) => {
                    RelaySubscriptionEndpointDisposition::Invalid
                }
                Err(
                    RelayEndpointRejection::PlaintextPublic
                    | RelayEndpointRejection::NonPublicAddress
                    | RelayEndpointRejection::Localhost,
                ) => RelaySubscriptionEndpointDisposition::Unsafe,
                Ok(()) => {
                    let normalized = TransportEndpoint(normalized_endpoint.clone());
                    if admitted_endpoints.contains(&normalized) {
                        RelaySubscriptionEndpointDisposition::Duplicate
                    } else if admitted_endpoints.len() >= self.max_endpoints_per_route {
                        RelaySubscriptionEndpointDisposition::BeyondRouteLimit
                    } else {
                        admitted_endpoints.push(normalized);
                        RelaySubscriptionEndpointDisposition::Admitted
                    }
                }
            };
            endpoint_outcomes.push(RelaySubscriptionEndpointOutcome {
                requested_endpoint,
                normalized_endpoint: Some(normalized_endpoint),
                disposition,
            });
        }

        RelaySubscriptionAdmission {
            requested_endpoints,
            admitted_endpoints,
            endpoint_outcomes,
        }
    }

    pub(crate) fn sanitize_publish_request(
        &self,
        mut request: TransportPublishRequest,
    ) -> Result<TransportPublishRequest, String> {
        match &mut request.target {
            TransportPublishTarget::Group { endpoints, .. } => {
                *endpoints = self.sanitize_endpoints(endpoints.clone(), "group publish")?;
            }
            TransportPublishTarget::Inbox { endpoints, .. } => {
                *endpoints = self.sanitize_endpoints(endpoints.clone(), "inbox publish")?;
            }
        }
        Ok(request)
    }

    /// Keep the endpoints that pass the host-safety rule and drop the rest.
    ///
    /// The counterpart to [`Self::sanitize_endpoints`] for endpoints that were
    /// *discovered* rather than configured — a relay list published by another
    /// account, say. Sanitizing is fail-closed because a configured relay set
    /// is the operator's stated intent, and one bad entry there is a mistake
    /// worth surfacing. A published list is untrusted input: rejecting all of
    /// it over one bad entry would let anyone make themselves unresolvable by
    /// appending a single loopback URL. Every endpoint is checked against the
    /// same rule; only the response to a rejection differs.
    ///
    /// The result still passes through [`Self::sanitize_endpoints`] at the
    /// dial chokepoint, so this narrows what is offered rather than replacing
    /// the check.
    pub(crate) fn retain_safe_endpoints(
        &self,
        endpoints: Vec<TransportEndpoint>,
        context: &str,
    ) -> Vec<TransportEndpoint> {
        let offered = endpoints.len();
        let mut kept: Vec<TransportEndpoint> = Vec::new();
        for endpoint in endpoints {
            let Ok(relay_url) = RelayUrl::parse(endpoint.as_str().trim()) else {
                continue;
            };
            if reject_unsafe_relay_host(&relay_url, self.allow_loopback).is_err() {
                continue;
            }
            let endpoint = TransportEndpoint(relay_url.to_string());
            if !kept.contains(&endpoint) {
                kept.push(endpoint);
            }
        }
        if kept.len() != offered {
            // Aggregate counts only: a rejected relay URL is somebody's
            // published data and never reaches a log line.
            tracing::debug!(
                target: "marmot_app::relay_plane",
                method = "retain_safe_endpoints",
                context = context,
                offered = offered,
                kept = kept.len(),
                "dropped discovered relay endpoints that failed the host-safety rule"
            );
        }
        kept
    }

    pub(crate) fn sanitize_endpoints(
        &self,
        endpoints: Vec<TransportEndpoint>,
        context: &str,
    ) -> Result<Vec<TransportEndpoint>, String> {
        let mut sanitized = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let raw = endpoint.as_str().trim();
            if raw.is_empty() {
                return Err(format!("{context}: invalid relay endpoint"));
            }
            let relay_url = RelayUrl::parse(raw)
                .map_err(|err| format!("{context}: invalid relay endpoint: {err}"))?;
            reject_unsafe_relay_host(&relay_url, self.allow_loopback)
                .map_err(|reason| format!("{context}: {reason}"))?;
            let endpoint = TransportEndpoint(relay_url.to_string());
            if !sanitized.contains(&endpoint) {
                sanitized.push(endpoint);
            }
        }
        if sanitized.len() > self.max_endpoints_per_route {
            return Err(format!(
                "{context}: relay endpoint count {} exceeds limit {}",
                sanitized.len(),
                self.max_endpoints_per_route
            ));
        }
        Ok(sanitized)
    }
}

/// Require TLS for every public relay. Plaintext `ws://` is admitted only for
/// an explicitly enabled loopback host; private/link-local/CGNAT and public
/// plaintext endpoints stay rejected even with the dev flag. Relay
/// endpoints arrive from signed routing components and relay-list events, so a
/// poisoned record must not steer the relay pool at internal services (SSRF;
/// see `docs/marmot-architecture/overview/dial-safety.md`). A `wss://` DOMAIN
/// host is accepted here: nostr-sdk owns DNS resolution and the WebSocket, so
/// resolve-time validation cannot be pinned at this layer — an accepted LOW
/// residual, per the dial-safety note. Error strings stay URL-free.
fn reject_unsafe_relay_host(url: &RelayUrl, allow_loopback: bool) -> Result<(), String> {
    evaluate_relay_url(url, allow_loopback).map_err(|rejection| rejection.reason().to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelayEndpointRejection {
    Invalid,
    Retired,
    PlaintextPublic,
    NonPublicAddress,
    Localhost,
}

impl RelayEndpointRejection {
    fn policy(self) -> RelayEndpointPolicy {
        match self {
            Self::Invalid => RelayEndpointPolicy::Invalid,
            Self::Retired => RelayEndpointPolicy::Retired,
            Self::PlaintextPublic | Self::NonPublicAddress | Self::Localhost => {
                RelayEndpointPolicy::Unsafe
            }
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::Invalid => "invalid relay endpoint",
            Self::Retired => "relay endpoint host is retired",
            Self::PlaintextPublic => {
                "plaintext relay endpoints are allowed only for loopback in dev mode"
            }
            Self::NonPublicAddress => "relay endpoint host is not a public address",
            Self::Localhost => "relay endpoint host must not be localhost",
        }
    }
}

fn evaluate_relay_url(url: &RelayUrl, allow_loopback: bool) -> Result<(), RelayEndpointRejection> {
    let parsed = Url::parse(url.as_str()).map_err(|_| RelayEndpointRejection::Invalid)?;
    let host = parsed.host().ok_or(RelayEndpointRejection::Invalid)?;
    if is_retired_relay_host(&host) {
        return Err(RelayEndpointRejection::Retired);
    }
    if parsed.scheme() == "ws" {
        return if allow_loopback && is_loopback_host(host) {
            Ok(())
        } else {
            Err(RelayEndpointRejection::PlaintextPublic)
        };
    }
    match host {
        Host::Ipv4(addr) => reject_non_public_ip(IpAddr::V4(addr), allow_loopback)
            .map_err(|_| RelayEndpointRejection::NonPublicAddress),
        Host::Ipv6(addr) => reject_non_public_ip(IpAddr::V6(addr), allow_loopback)
            .map_err(|_| RelayEndpointRejection::NonPublicAddress),
        Host::Domain(domain) => {
            if is_loopback_host(Host::Domain(domain)) && !allow_loopback {
                return Err(RelayEndpointRejection::Localhost);
            }
            Ok(())
        }
    }
}

fn is_retired_relay_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.');
            RETIRED_RELAY_HOSTS
                .iter()
                .any(|retired| domain.eq_ignore_ascii_case(retired))
        }
        Host::Ipv4(_) | Host::Ipv6(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoints(urls: &[&str]) -> Vec<TransportEndpoint> {
        urls.iter()
            .map(|url| TransportEndpoint((*url).to_owned()))
            .collect()
    }

    /// Relay lists published by other accounts are input, not configuration.
    /// One hostile or malformed entry in somebody's NIP-65 list must not deny
    /// every other relay they publish -- otherwise anyone could make themselves
    /// unresolvable, or take the outbox path down for everyone, by adding a
    /// single bad URL. The host rule applied to each entry is the same one
    /// `sanitize_endpoints` enforces; only the answer to a rejection differs.
    #[test]
    fn discovered_endpoints_drop_the_unsafe_and_keep_the_rest() {
        let policy = RelaySafetyPolicy::default();

        let kept = policy.retain_safe_endpoints(
            endpoints(&[
                "wss://good.example",
                "ws://127.0.0.1:8080",
                "not a url",
                "wss://also-good.example",
                "ws://169.254.169.254",
            ]),
            "test",
        );

        assert_eq!(
            kept,
            endpoints(&["wss://good.example", "wss://also-good.example"])
        );
    }

    #[test]
    fn discovered_endpoints_are_deduplicated() {
        let policy = RelaySafetyPolicy::default();

        let kept = policy.retain_safe_endpoints(
            endpoints(&["wss://relay.example", "wss://relay.example"]),
            "test",
        );

        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn nostr_band_is_rejected_at_the_relay_plane_boundary() {
        let policy = RelaySafetyPolicy::default();
        for endpoint in [
            "wss://relay.nostr.band",
            "wss://RELAY.NOSTR.BAND./path",
            "wss://relay.nostr.band:443/alternate",
        ] {
            let offered = endpoints(&[endpoint, "wss://good.example"]);
            assert!(
                policy.sanitize_endpoints(offered.clone(), "test").is_err(),
                "configured routes must fail closed when they contain a retired relay"
            );
            assert_eq!(
                policy.retain_safe_endpoints(offered, "test"),
                endpoints(&["wss://good.example"]),
                "discovered routes must drop retired relays and keep safe siblings"
            );
        }
    }

    #[test]
    fn damus_is_eligible_under_the_normal_dial_policy() {
        let policy = RelaySafetyPolicy::default();
        for endpoint in ["wss://relay.damus.io", "wss://RELAY.DAMUS.IO./path"] {
            assert_eq!(
                policy
                    .sanitize_endpoints(endpoints(&[endpoint]), "test")
                    .expect("Damus should pass the normal TLS and host-safety policy")
                    .len(),
                1
            );
            assert_eq!(
                policy.classify_endpoints(vec![endpoint.to_owned()])[0].policy,
                RelayEndpointPolicy::Allowed
            );
        }
        assert_eq!(
            policy.classify_endpoints(vec!["ws://relay.damus.io".to_owned()])[0].policy,
            RelayEndpointPolicy::Unsafe,
            "public plaintext WebSockets stay forbidden"
        );
    }

    #[test]
    fn relay_endpoint_classifier_uses_the_dial_policy() {
        let policy = RelaySafetyPolicy::default();
        let classified = policy.classify_endpoints(vec![
            " wss://relay.example ".to_owned(),
            "wss://RELAY.DAMUS.IO./path".to_owned(),
            "wss://RELAY.NOSTR.BAND./path".to_owned(),
            "not a relay".to_owned(),
            "ws://relay.example".to_owned(),
            "wss://127.0.0.1".to_owned(),
        ]);

        assert_eq!(
            classified
                .iter()
                .map(|result| result.policy)
                .collect::<Vec<_>>(),
            vec![
                RelayEndpointPolicy::Allowed,
                RelayEndpointPolicy::Allowed,
                RelayEndpointPolicy::Retired,
                RelayEndpointPolicy::Invalid,
                RelayEndpointPolicy::Unsafe,
                RelayEndpointPolicy::Unsafe,
            ]
        );
        assert_eq!(classified[0].endpoint, " wss://relay.example ");
        assert!(classified[0].normalized_endpoint.is_some());
        assert!(classified[1].normalized_endpoint.is_some());
        assert!(classified[2].normalized_endpoint.is_some());
        assert_eq!(classified[3].normalized_endpoint, None);
    }

    #[test]
    fn relay_endpoint_classifier_respects_the_loopback_dev_opt_in() {
        let classified = RelaySafetyPolicy::with_allow_loopback(true)
            .classify_endpoints(vec!["ws://localhost:8080".to_owned()]);

        assert_eq!(classified[0].policy, RelayEndpointPolicy::Allowed);
    }

    #[test]
    fn retired_relay_host_list_is_stable_and_scheme_free() {
        assert_eq!(retired_relay_hosts(), vec!["relay.nostr.band"]);
    }

    /// A published list of only unsafe hosts yields nothing, rather than
    /// falling back to anything the caller did not ask for.
    #[test]
    fn discovered_endpoints_can_all_be_dropped() {
        let policy = RelaySafetyPolicy::default();

        assert!(
            policy
                .retain_safe_endpoints(endpoints(&["ws://127.0.0.1", "ws://10.0.0.1"]), "test")
                .is_empty()
        );
    }

    #[test]
    fn rejects_non_public_relay_hosts_by_default() {
        let policy = RelaySafetyPolicy::default();
        for url in [
            "ws://127.0.0.1:8080",
            "ws://10.0.0.1",
            "ws://169.254.169.254",
            "ws://[::1]:8080",
            "ws://[fc00::1]",
            "ws://localhost:7777",
            // Rooted localhost names still resolve to loopback and must be
            // rejected too (they parse as `Host::Domain("localhost.")`).
            "ws://localhost.:7777",
            "ws://dev.localhost.:7777",
        ] {
            assert!(
                policy
                    .sanitize_endpoints(endpoints(&[url]), "test")
                    .is_err(),
                "{url} must be rejected"
            );
        }
    }

    #[test]
    fn accepts_public_relay_hosts_regardless_of_opt_in() {
        for policy in [
            RelaySafetyPolicy::default(),
            RelaySafetyPolicy::with_allow_loopback(true),
        ] {
            let sanitized = policy
                .sanitize_endpoints(endpoints(&["wss://relay.example"]), "test")
                .expect("public relay accepted");
            assert_eq!(sanitized.len(), 1);
        }
    }

    #[test]
    fn rejects_public_plaintext_relays_even_with_dev_opt_in() {
        for policy in [
            RelaySafetyPolicy::default(),
            RelaySafetyPolicy::with_allow_loopback(true),
        ] {
            for url in ["ws://relay.example", "ws://8.8.8.8"] {
                assert!(
                    policy
                        .sanitize_endpoints(endpoints(&[url]), "test")
                        .is_err(),
                    "{url} must require TLS"
                );
            }
        }
    }

    #[test]
    fn dev_opt_in_admits_loopback_but_not_private_ranges() {
        let policy = RelaySafetyPolicy::with_allow_loopback(true);
        for url in ["ws://127.0.0.1:8080", "ws://[::1]:8080", "ws://localhost"] {
            assert!(
                policy.sanitize_endpoints(endpoints(&[url]), "test").is_ok(),
                "{url} must be accepted under the dev opt-in"
            );
        }
        // The opt-in opens loopback only; private/link-local literals stay
        // rejected even in dev mode.
        for url in ["ws://10.0.0.1", "ws://169.254.169.254"] {
            assert!(
                policy
                    .sanitize_endpoints(endpoints(&[url]), "test")
                    .is_err(),
                "{url} must stay rejected even with the dev opt-in"
            );
        }
    }

    #[test]
    fn count_cap_still_enforced() {
        let policy = RelaySafetyPolicy::default();
        let many: Vec<String> = (0..20).map(|i| format!("wss://relay{i}.example")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(policy.sanitize_endpoints(endpoints(&refs), "test").is_err());
    }

    #[test]
    fn subscription_admission_isolates_rejections_and_preserves_source_order() {
        let policy = RelaySafetyPolicy::default();
        let admission = policy.admit_subscription_endpoints(endpoints(&[
            "wss://one.example",
            "not a relay",
            "wss://relay.nostr.band",
            "ws://relay.example",
            "wss://one.example",
            "wss://two.example",
        ]));

        assert_eq!(
            admission.admitted_endpoints,
            endpoints(&["wss://one.example", "wss://two.example"])
        );
        assert_eq!(
            admission
                .endpoint_outcomes
                .iter()
                .map(|outcome| outcome.disposition)
                .collect::<Vec<_>>(),
            vec![
                RelaySubscriptionEndpointDisposition::Admitted,
                RelaySubscriptionEndpointDisposition::Invalid,
                RelaySubscriptionEndpointDisposition::Retired,
                RelaySubscriptionEndpointDisposition::Unsafe,
                RelaySubscriptionEndpointDisposition::Duplicate,
                RelaySubscriptionEndpointDisposition::Admitted,
            ]
        );
    }

    #[test]
    fn subscription_admission_caps_allowed_distinct_endpoints_without_failing_route() {
        let policy = RelaySafetyPolicy::default();
        let requested = (0..18)
            .map(|index| TransportEndpoint(format!("wss://relay{index}.example")))
            .collect::<Vec<_>>();
        let admission = policy.admit_subscription_endpoints(requested);

        assert_eq!(admission.admitted_endpoints.len(), 16);
        assert_eq!(
            admission
                .endpoint_outcomes
                .iter()
                .filter(|outcome| {
                    outcome.disposition == RelaySubscriptionEndpointDisposition::BeyondRouteLimit
                })
                .count(),
            2
        );
    }

    #[test]
    fn subscription_admission_can_report_an_entirely_blocked_route() {
        let admission = RelaySafetyPolicy::default().admit_subscription_endpoints(endpoints(&[
            "not a relay",
            "wss://relay.nostr.band",
            "ws://10.0.0.1",
        ]));

        assert!(admission.admitted_endpoints.is_empty());
        assert_eq!(admission.endpoint_outcomes.len(), 3);
    }
}
