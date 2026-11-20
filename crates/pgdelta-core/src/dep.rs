//! Dependency-ordered statement planning: a small graph with a
//! deterministic topological sort.
//!
//! The graph is generic: nodes carry a payload, a rank (broad phase),
//! and a sort key (tie-break within a rank). Edges encode "must come
//! first" relations. The sort is Kahn's algorithm seeded by `(rank,
//! key)` order, which makes the output deterministic; a cycle is an
//! internal error (a planning bug), never a user condition.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// A node in the planning graph.
#[derive(Debug, Clone)]
pub(crate) struct PlanNode<T> {
    /// Broad phase: lower ranks sort first.
    pub rank: u32,
    /// Tie-break within a rank.
    pub key: String,
    /// The payload (statement builder).
    pub payload: T,
    /// Other node keys this node must come **after** (the referenced
    /// nodes sort first).
    pub after: Vec<String>,
}

/// Deterministically topologically sorts `nodes`.
///
/// Every node key must be unique; edges referencing unknown keys are an
/// internal error.
pub(crate) fn toposort<T>(nodes: Vec<PlanNode<T>>) -> Result<Vec<T>> {
    let mut index: BTreeMap<String, usize> = BTreeMap::new();
    for (i, node) in nodes.iter().enumerate() {
        if index.insert(node.key.clone(), i).is_some() {
            return Err(Error::internal(format!(
                "duplicate plan node key {:?}",
                node.key
            )));
        }
    }

    // Pre-compute the deterministic seed order (rank, then key).
    let mut seeded: Vec<usize> = (0..nodes.len()).collect();
    seeded.sort_by(|&a, &b| {
        nodes[a]
            .rank
            .cmp(&nodes[b].rank)
            .then_with(|| nodes[a].key.cmp(&nodes[b].key))
    });

    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut remaining_deps: Vec<usize> = vec![0; nodes.len()];
    for node in &nodes {
        let me = index[&node.key];
        for after in &node.after {
            let Some(&before) = index.get(after) else {
                return Err(Error::internal(format!(
                    "plan edge references unknown node {after:?}"
                )));
            };
            dependents[before].push(me);
            remaining_deps[me] += 1;
        }
    }

    // Ready set ordered by (rank, key).
    let key_of = |i: usize| (nodes[i].rank, nodes[i].key.clone());
    let mut ready: Vec<usize> = seeded
        .iter()
        .copied()
        .filter(|&i| remaining_deps[i] == 0)
        .collect();
    ready.sort_by_key(|&i| key_of(i));

    let mut order: Vec<usize> = Vec::with_capacity(nodes.len());
    while let Some(i) = ready.first().copied() {
        ready.remove(0);
        order.push(i);
        for &dep in &dependents[i] {
            remaining_deps[dep] -= 1;
            if remaining_deps[dep] == 0 {
                let position = ready
                    .binary_search_by(|&probe| key_of(probe).cmp(&key_of(dep)))
                    .unwrap_or_else(|pos| pos);
                ready.insert(position, dep);
            }
        }
    }
    if order.len() != nodes.len() {
        return Err(Error::internal(
            "dependency cycle in the plan graph — this is a pgdelta bug".to_owned(),
        ));
    }

    // Consume the payloads in the computed order.
    let mut slots: Vec<Option<T>> = nodes.into_iter().map(|n| Some(n.payload)).collect();
    let mut out = Vec::with_capacity(order.len());
    for i in order {
        // The index built above matches the payload positions by
        // construction, so this swap never fails.
        let payload = slots[i].take();
        out.push(payload.ok_or_else(|| Error::internal("plan node consumed twice".to_owned()))?);
    }
    Ok(out)
}
