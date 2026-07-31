//! Reconciling the running tag set with a newly delivered one.
//!
//! Tearing the session down and rebuilding every MonitoredItem on each config
//! change is the simple option, but at 250 tags it costs several seconds of
//! blind time and a burst of allocations. Diffing lets an edit of a handful of
//! tags cost a handful of service calls.

use std::collections::BTreeMap;

use crate::bundle::TagSpec;

/// The work needed to move from the running tag set to the desired one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiffPlan {
    /// Tags with no MonitoredItem yet.
    pub add: Vec<TagSpec>,
    /// Addresses whose MonitoredItem must be deleted.
    pub remove: Vec<String>,
    /// Tags whose sampling parameters changed. Because the sampling interval
    /// determines which subscription an item belongs to, these are handled as
    /// a delete followed by a create rather than `ModifyMonitoredItems`.
    pub modify: Vec<TagSpec>,
}

impl DiffPlan {
    /// True when the running configuration already matches the desired one.
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty() && self.modify.is_empty()
    }

    /// Number of service operations the plan implies.
    pub fn len(&self) -> usize {
        self.add.len() + self.remove.len() + self.modify.len()
    }
}

/// Computes the plan to get from `current` to `desired`.
///
/// Both sides are keyed by bare address, which is also the telemetry key, so a
/// tag that merely moved between groups is correctly seen as a modification
/// rather than a remove/add pair with a gap in between.
pub fn diff(current: &[TagSpec], desired: &[TagSpec]) -> DiffPlan {
    let current: BTreeMap<&str, &TagSpec> =
        current.iter().map(|t| (t.address.as_str(), t)).collect();
    let desired_map: BTreeMap<&str, &TagSpec> =
        desired.iter().map(|t| (t.address.as_str(), t)).collect();

    let mut plan = DiffPlan::default();

    // Preserve the bundle's ordering for adds and modifies: it keeps chunked
    // CreateMonitoredItems requests grouped by subscription.
    for tag in desired {
        match current.get(tag.address.as_str()) {
            None => plan.add.push(tag.clone()),
            Some(existing) if needs_recreate(existing, tag) => plan.modify.push(tag.clone()),
            Some(_) => {}
        }
    }

    for address in current.keys() {
        if !desired_map.contains_key(address) {
            plan.remove.push((*address).to_string());
        }
    }

    plan
}

/// Anything that changes the item's identity on the server forces a recreate.
fn needs_recreate(current: &TagSpec, desired: &TagSpec) -> bool {
    current.node_id != desired.node_id
        || current.scan_rate_ms != desired.scan_rate_ms
        || current.deadband != desired.deadband
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(addr: &str, rate: u32) -> TagSpec {
        TagSpec {
            address: addr.into(),
            node_id: format!("ns=2;s={addr}"),
            scan_rate_ms: rate,
            deadband: None,
        }
    }

    #[test]
    fn identical_sets_produce_no_work() {
        let a = vec![tag("x", 1000), tag("y", 250)];
        let plan = diff(&a, &a);
        assert!(plan.is_empty());
        assert_eq!(plan.len(), 0);
    }

    #[test]
    fn new_tags_are_added_in_bundle_order() {
        let plan = diff(&[tag("x", 1000)], &[tag("x", 1000), tag("a", 250), tag("b", 250)]);
        assert_eq!(
            plan.add.iter().map(|t| t.address.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(plan.remove.is_empty());
        assert!(plan.modify.is_empty());
    }

    #[test]
    fn missing_tags_are_removed() {
        let plan = diff(&[tag("x", 1000), tag("y", 1000)], &[tag("x", 1000)]);
        assert_eq!(plan.remove, vec!["y".to_string()]);
        assert!(plan.add.is_empty());
    }

    #[test]
    fn a_scan_rate_change_is_a_modification() {
        let plan = diff(&[tag("x", 1000)], &[tag("x", 250)]);
        assert!(plan.add.is_empty());
        assert!(plan.remove.is_empty());
        assert_eq!(plan.modify.len(), 1);
        assert_eq!(plan.modify[0].scan_rate_ms, 250);
    }

    #[test]
    fn a_node_id_change_under_the_same_address_is_a_modification() {
        let mut moved = tag("x", 1000);
        moved.node_id = "ns=3;s=x".into();
        let plan = diff(&[tag("x", 1000)], &[moved]);
        assert_eq!(plan.modify.len(), 1);
    }

    #[test]
    fn a_deadband_change_is_a_modification() {
        let mut with_deadband = tag("x", 1000);
        with_deadband.deadband = Some(0.5);
        let plan = diff(&[tag("x", 1000)], &[with_deadband]);
        assert_eq!(plan.modify.len(), 1);
    }

    #[test]
    fn from_empty_everything_is_an_add() {
        let desired = vec![tag("a", 250), tag("b", 250)];
        let plan = diff(&[], &desired);
        assert_eq!(plan.add.len(), 2);
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn to_empty_everything_is_a_remove() {
        let plan = diff(&[tag("a", 250), tag("b", 250)], &[]);
        assert_eq!(plan.remove, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn mixed_edit() {
        let current = vec![tag("keep", 1000), tag("change", 1000), tag("gone", 1000)];
        let desired = vec![tag("keep", 1000), tag("change", 250), tag("new", 500)];
        let plan = diff(&current, &desired);
        assert_eq!(plan.add.len(), 1);
        assert_eq!(plan.add[0].address, "new");
        assert_eq!(plan.modify.len(), 1);
        assert_eq!(plan.modify[0].address, "change");
        assert_eq!(plan.remove, vec!["gone".to_string()]);
    }
}
