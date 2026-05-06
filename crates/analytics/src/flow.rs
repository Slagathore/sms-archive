//! Conversation-flow Sankey diagram data.
//!
//! Mimoto's "Conversation Flow" panel is a 4-column Sankey:
//!
//! ```text
//!   col 0          col 1               col 2                col 3
//!   ┌──────────┐   ┌──────────────┐    ┌─────────────────┐  ┌───────────┐
//!   │ Started  │ → │ Big moments  │ →  │ Major contrib.  │→ │ Final     │
//!   │ by: Me   │   │ Everyday     │    │ was Me / Them   │  │ reply by  │
//!   │ Started  │   │ No reply     │    │   (skipped for  │  │ Me / Them │
//!   │ by: Them │   │  (missed)    │    │   missed)       │  │           │
//!   └──────────┘   └──────────────┘    └─────────────────┘  └───────────┘
//! ```
//!
//! Each conversation traces a path through these columns; we count the
//! frequency of each unique path and emit nodes (one per cell) and links
//! (one per path segment with a non-zero count).
//!
//! Missed conversations have no major contributor and no real "final reply"
//! — they terminate at column 1 with the `no_reply` category. The renderer
//! treats them as a dead-end. The other side's "missed" classification is
//! still informative ("you started but they never responded") so we keep
//! the start-by-no-reply link.
//!
//! # Big-moment classification for the Sankey
//!
//! A conversation flows through the `big_moment` column-1 node if EITHER
//! `is_big_moment_static` OR `is_big_moment_dynamic` is set. Using OR keeps
//! the picture simple and gives credit to "this was a long convo by your
//! standards with this person, even if it wasn't long absolutely" cases.

use crate::types::{Conversation, Participant};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One node in the Sankey diagram. `id` is stable across the whole renderer
/// so links can reference it; `label` is the user-facing text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SankeyNode {
    pub id: String,
    pub label: String,
    /// Total count of conversations passing through this node.
    pub value: u32,
    /// 0-3, indicating the column the node belongs to.
    pub column: u8,
}

/// One link between two adjacent-column nodes. `value` = how many
/// conversations followed this exact source→target edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SankeyLink {
    pub source: String,
    pub target: String,
    pub value: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SankeyData {
    pub nodes: Vec<SankeyNode>,
    pub links: Vec<SankeyLink>,
}

/// Compute the Sankey path counts from segmented conversations.
///
/// Returns nodes in column-then-id order and links sorted by `(source, target)`
/// for deterministic test output.
pub fn build_conversation_flow(conversations: &[Conversation]) -> SankeyData {
    if conversations.is_empty() {
        return SankeyData::default();
    }

    // Aggregate counts.
    let mut node_counts: HashMap<String, u32> = HashMap::new();
    let mut link_counts: HashMap<(String, String), u32> = HashMap::new();

    for conv in conversations {
        let started_id = match conv.started_by {
            Participant::Me => "started_me",
            Participant::Them => "started_them",
        };
        *node_counts.entry(started_id.to_string()).or_insert(0) += 1;

        // Column 1: category.
        let category_id = if conv.is_missed {
            "no_reply"
        } else if conv.is_big_moment_static || conv.is_big_moment_dynamic {
            "big_moment"
        } else {
            "everyday"
        };
        *node_counts.entry(category_id.to_string()).or_insert(0) += 1;
        *link_counts
            .entry((started_id.to_string(), category_id.to_string()))
            .or_insert(0) += 1;

        if conv.is_missed {
            // Path terminates here. No contributor, no final reply.
            continue;
        }

        // Column 2: major contributor.
        let contrib_id = match conv.major_contributor {
            Participant::Me => "contrib_me",
            Participant::Them => "contrib_them",
        };
        *node_counts.entry(contrib_id.to_string()).or_insert(0) += 1;
        *link_counts
            .entry((category_id.to_string(), contrib_id.to_string()))
            .or_insert(0) += 1;

        // Column 3: final reply.
        let final_id = match conv.final_reply_by {
            Participant::Me => "final_me",
            Participant::Them => "final_them",
        };
        *node_counts.entry(final_id.to_string()).or_insert(0) += 1;
        *link_counts
            .entry((contrib_id.to_string(), final_id.to_string()))
            .or_insert(0) += 1;
    }

    // Materialize nodes with labels and column assignments.
    let mut nodes: Vec<SankeyNode> = node_counts
        .into_iter()
        .map(|(id, value)| {
            let (column, label) = node_metadata(&id);
            SankeyNode {
                id,
                label: label.to_string(),
                value,
                column,
            }
        })
        .collect();
    nodes.sort_by(|a, b| a.column.cmp(&b.column).then_with(|| a.id.cmp(&b.id)));

    // Materialize links.
    let mut links: Vec<SankeyLink> = link_counts
        .into_iter()
        .map(|((source, target), value)| SankeyLink { source, target, value })
        .collect();
    links.sort_by(|a, b| a.source.cmp(&b.source).then_with(|| a.target.cmp(&b.target)));

    SankeyData { nodes, links }
}

/// Map a node ID to its (column, label). Stable mapping that the renderer
/// relies on. The label uses generic "you" / "them" wording — the dashboard
/// can replace those at render time with real names if it wants.
fn node_metadata(id: &str) -> (u8, &'static str) {
    match id {
        "started_me" => (0, "Started by you"),
        "started_them" => (0, "Started by them"),
        "big_moment" => (1, "Big moment"),
        "everyday" => (1, "Everyday chat"),
        "no_reply" => (1, "No reply"),
        "contrib_me" => (2, "You contributed more"),
        "contrib_them" => (2, "They contributed more"),
        "final_me" => (3, "Final reply: you"),
        "final_them" => (3, "Final reply: them"),
        _ => (255, "Unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convo(
        started: Participant,
        final_reply: Participant,
        major: Participant,
        is_missed: bool,
        is_big: bool,
    ) -> Conversation {
        Conversation {
            contact_id: "c1".to_string(),
            start_time_ms: 0,
            end_time_ms: 100,
            started_by: started,
            final_reply_by: final_reply,
            my_message_count: 1,
            their_message_count: if is_missed { 0 } else { 1 },
            total_message_count: if is_missed { 1 } else { 2 },
            major_contributor: major,
            is_missed,
            missed_by: if is_missed { Some(started.flip()) } else { None },
            is_big_moment_static: is_big,
            is_big_moment_dynamic: false,
            reconnect_tier: 0,
            points: 0.0,
        }
    }

    fn find_node<'a>(d: &'a SankeyData, id: &str) -> Option<&'a SankeyNode> {
        d.nodes.iter().find(|n| n.id == id)
    }

    fn find_link<'a>(d: &'a SankeyData, src: &str, tgt: &str) -> Option<&'a SankeyLink> {
        d.links.iter().find(|l| l.source == src && l.target == tgt)
    }

    #[test]
    fn empty_input_yields_empty_flow() {
        let data = build_conversation_flow(&[]);
        assert!(data.nodes.is_empty());
        assert!(data.links.is_empty());
    }

    #[test]
    fn single_complete_convo_traces_one_path() {
        let convs = vec![convo(
            Participant::Them,    // they started
            Participant::Me,      // I had final reply
            Participant::Me,      // I contributed more
            false,                // not missed
            false,                // not big
        )];
        let data = build_conversation_flow(&convs);
        // Path: started_them → everyday → contrib_me → final_me
        assert_eq!(find_node(&data, "started_them").unwrap().value, 1);
        assert_eq!(find_node(&data, "everyday").unwrap().value, 1);
        assert_eq!(find_node(&data, "contrib_me").unwrap().value, 1);
        assert_eq!(find_node(&data, "final_me").unwrap().value, 1);
        // Other column-0 nodes should not be present.
        assert!(find_node(&data, "started_me").is_none());
        // Links along the path.
        assert_eq!(find_link(&data, "started_them", "everyday").unwrap().value, 1);
        assert_eq!(find_link(&data, "everyday", "contrib_me").unwrap().value, 1);
        assert_eq!(find_link(&data, "contrib_me", "final_me").unwrap().value, 1);
    }

    #[test]
    fn missed_convo_terminates_at_no_reply() {
        let convs = vec![convo(
            Participant::Me,
            Participant::Me,        // never replied to themselves but final = same
            Participant::Me,
            true,                   // missed
            false,
        )];
        let data = build_conversation_flow(&convs);
        // Path: started_me → no_reply, then stops.
        assert_eq!(find_node(&data, "started_me").unwrap().value, 1);
        assert_eq!(find_node(&data, "no_reply").unwrap().value, 1);
        assert!(find_node(&data, "contrib_me").is_none());
        assert!(find_node(&data, "final_me").is_none());
        // Only one link should exist.
        assert_eq!(data.links.len(), 1);
        assert_eq!(find_link(&data, "started_me", "no_reply").unwrap().value, 1);
    }

    #[test]
    fn big_moment_routes_through_big_moment_node() {
        let convs = vec![convo(
            Participant::Me,
            Participant::Them,
            Participant::Them,
            false,
            true,    // big moment
        )];
        let data = build_conversation_flow(&convs);
        assert_eq!(find_node(&data, "big_moment").unwrap().value, 1);
        assert!(find_node(&data, "everyday").is_none());
        assert_eq!(find_link(&data, "started_me", "big_moment").unwrap().value, 1);
    }

    #[test]
    fn dynamic_big_moment_alone_routes_through_big_moment() {
        // Static off, dynamic on — should still classify as big_moment.
        let mut c = convo(
            Participant::Me,
            Participant::Them,
            Participant::Them,
            false,
            false,    // static OFF
        );
        c.is_big_moment_dynamic = true;    // dynamic ON
        let data = build_conversation_flow(&[c]);
        assert_eq!(find_node(&data, "big_moment").unwrap().value, 1);
    }

    #[test]
    fn aggregates_multiple_convos_with_same_path() {
        // Three convos all following the same exact path. Each node and link
        // along the path should have value = 3.
        let c = convo(
            Participant::Them,
            Participant::Me,
            Participant::Me,
            false,
            false,
        );
        let convs = vec![c.clone(), c.clone(), c];
        let data = build_conversation_flow(&convs);
        assert_eq!(find_node(&data, "started_them").unwrap().value, 3);
        assert_eq!(find_node(&data, "everyday").unwrap().value, 3);
        assert_eq!(find_node(&data, "contrib_me").unwrap().value, 3);
        assert_eq!(find_node(&data, "final_me").unwrap().value, 3);
        assert_eq!(find_link(&data, "started_them", "everyday").unwrap().value, 3);
    }

    #[test]
    fn diverging_paths_split_links_correctly() {
        // Two convos: same start, then one is everyday, the other is big_moment.
        let everyday = convo(
            Participant::Me,
            Participant::Them,
            Participant::Them,
            false,
            false,
        );
        let big = convo(
            Participant::Me,
            Participant::Them,
            Participant::Them,
            false,
            true,
        );
        let data = build_conversation_flow(&[everyday, big]);
        // started_me appears in both → value 2.
        assert_eq!(find_node(&data, "started_me").unwrap().value, 2);
        // Two divergent links from started_me.
        assert_eq!(find_link(&data, "started_me", "everyday").unwrap().value, 1);
        assert_eq!(find_link(&data, "started_me", "big_moment").unwrap().value, 1);
    }

    #[test]
    fn nodes_are_sorted_by_column_then_id() {
        let convs = vec![
            convo(Participant::Me, Participant::Them, Participant::Me, false, false),
            convo(Participant::Them, Participant::Me, Participant::Them, false, true),
            convo(Participant::Me, Participant::Me, Participant::Me, true, false),
        ];
        let data = build_conversation_flow(&convs);
        // All column-0 nodes come before column-1, etc.
        let cols: Vec<u8> = data.nodes.iter().map(|n| n.column).collect();
        let mut sorted = cols.clone();
        sorted.sort();
        assert_eq!(cols, sorted, "nodes must be sorted by column ascending");
    }
}
