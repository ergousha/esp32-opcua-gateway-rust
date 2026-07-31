//! Turning a flat tag list into the OPC UA service calls that realise it.
//!
//! Two constraints shape this:
//!
//! * A subscription has exactly one publishing interval, so tags are grouped by
//!   scan rate — one subscription per distinct rate. Keeping the number of
//!   distinct rates small is the single biggest lever on memory use.
//! * `CreateMonitoredItems` for 250 items in one request produces a message
//!   larger than the device's chunk budget and a matching response allocation.
//!   Requests are therefore chunked.
//!
//! This module is pure: it decides *what* to call, the firmware performs the
//! calls.

use crate::bundle::TagSpec;

/// Monitored items requested per `CreateMonitoredItems` call.
///
/// Sized so that a request and its response both stay well inside the tuned
/// `max_chunk_size`/`max_message_size` on the device.
pub const MAX_ITEMS_PER_REQUEST: usize = 50;

/// Client handles are 1-based; 0 means "let the stack allocate one", which
/// would break the handle-to-address mapping.
pub const FIRST_CLIENT_HANDLE: u32 = 1;

/// One tag with the client handle that identifies it in notifications.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedItem {
    /// Handle echoed back by the server on every notification.
    pub client_handle: u32,
    /// The tag this handle refers to.
    pub tag: TagSpec,
}

/// One subscription and the chunked create-requests that populate it.
#[derive(Debug, Clone, PartialEq)]
pub struct SubscriptionPlan {
    /// Publishing interval / sampling interval, in milliseconds.
    pub scan_rate_ms: u32,
    /// Item batches, each one `CreateMonitoredItems` call.
    pub chunks: Vec<Vec<PlannedItem>>,
}

impl SubscriptionPlan {
    /// Total items across all chunks.
    pub fn item_count(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }
}

/// Allocates client handles and groups tags into per-rate, chunked plans.
///
/// Handles are allocated over the whole tag list (not per subscription) so a
/// single flat handle-to-address map serves every subscription.
pub fn plan(tags: &[TagSpec], chunk_size: usize) -> Vec<SubscriptionPlan> {
    let chunk_size = chunk_size.max(1);

    let mut rates: Vec<u32> = tags.iter().map(|t| t.scan_rate_ms).collect();
    rates.sort_unstable();
    rates.dedup();

    let mut next_handle = FIRST_CLIENT_HANDLE;
    let mut plans = Vec::with_capacity(rates.len());

    for rate in rates {
        let items: Vec<PlannedItem> = tags
            .iter()
            .filter(|t| t.scan_rate_ms == rate)
            .map(|t| {
                let item = PlannedItem {
                    client_handle: next_handle,
                    tag: t.clone(),
                };
                next_handle += 1;
                item
            })
            .collect();

        plans.push(SubscriptionPlan {
            scan_rate_ms: rate,
            chunks: items
                .chunks(chunk_size)
                .map(<[PlannedItem]>::to_vec)
                .collect(),
        });
    }

    plans
}

/// Number of `CreateMonitoredItems` calls a plan implies.
pub fn request_count(plans: &[SubscriptionPlan]) -> usize {
    plans.iter().map(|p| p.chunks.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_TAGS;

    fn tag(addr: &str, rate: u32) -> TagSpec {
        TagSpec {
            address: addr.into(),
            node_id: format!("ns=2;s={addr}"),
            scan_rate_ms: rate,
            deadband: None,
        }
    }

    #[test]
    fn one_subscription_per_distinct_scan_rate_ascending() {
        let tags = vec![tag("a", 1000), tag("b", 250), tag("c", 1000), tag("d", 250)];
        let plans = plan(&tags, 10);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].scan_rate_ms, 250);
        assert_eq!(plans[1].scan_rate_ms, 1000);
        assert_eq!(plans[0].item_count(), 2);
        assert_eq!(plans[1].item_count(), 2);
    }

    #[test]
    fn client_handles_are_unique_and_start_at_one() {
        let tags: Vec<_> = (0..20)
            .map(|i| tag(&format!("t{i}"), if i % 2 == 0 { 250 } else { 1000 }))
            .collect();
        let plans = plan(&tags, 3);

        let mut handles: Vec<u32> = plans
            .iter()
            .flat_map(|p| p.chunks.iter().flatten())
            .map(|i| i.client_handle)
            .collect();
        handles.sort_unstable();
        assert_eq!(handles, (1..=20).collect::<Vec<_>>());
    }

    #[test]
    fn requests_are_chunked() {
        let tags: Vec<_> = (0..MAX_TAGS).map(|i| tag(&format!("t{i}"), 1000)).collect();
        let plans = plan(&tags, MAX_ITEMS_PER_REQUEST);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].chunks.len(), MAX_TAGS.div_ceil(MAX_ITEMS_PER_REQUEST));
        assert!(plans[0].chunks.iter().all(|c| c.len() <= MAX_ITEMS_PER_REQUEST));
        assert_eq!(plans[0].item_count(), MAX_TAGS);
        assert_eq!(request_count(&plans), 5);
    }

    #[test]
    fn a_short_final_chunk_is_kept() {
        let tags: Vec<_> = (0..7).map(|i| tag(&format!("t{i}"), 1000)).collect();
        let plans = plan(&tags, 3);
        assert_eq!(
            plans[0].chunks.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![3, 3, 1]
        );
    }

    #[test]
    fn tag_order_within_a_rate_is_preserved() {
        let tags = vec![tag("z", 1000), tag("a", 1000), tag("m", 1000)];
        let plans = plan(&tags, 10);
        let addrs: Vec<_> = plans[0]
            .chunks
            .iter()
            .flatten()
            .map(|i| i.tag.address.as_str())
            .collect();
        assert_eq!(addrs, vec!["z", "a", "m"]);
    }

    #[test]
    fn an_empty_tag_list_plans_nothing() {
        assert!(plan(&[], 10).is_empty());
        assert_eq!(request_count(&[]), 0);
    }

    #[test]
    fn a_zero_chunk_size_does_not_loop_forever() {
        let plans = plan(&[tag("a", 1000)], 0);
        assert_eq!(plans[0].chunks.len(), 1);
    }
}
