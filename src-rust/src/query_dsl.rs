//! JSON Query DSL evaluator.
//!
//! Evaluates read-only declarative JSON queries against the in-memory graph.
//! Supports: match (filter), traverse (graph walk), where (edge constraints),
//! aggregate (group_by + count), and result limiting.

use crate::graph::MemoryEngine;
use crate::types::*;
use std::collections::{HashMap, HashSet, VecDeque};

/// Evaluate a DSL query against the memory engine, returning matching results as JSON.
pub fn evaluate_query(engine: &MemoryEngine, query: &DslQuery) -> serde_json::Value {
    // Step 1: Gather initial candidate nodes via match clause
    let candidates = match_nodes(engine, &query.match_clause);

    // Step 2: If there's a where clause, filter candidates by edge constraints
    let filtered = if let Some(ref where_clause) = query.where_clause {
        apply_where_filter(engine, candidates, where_clause)
    } else {
        candidates
    };

    // Step 3: If there's a traverse clause, expand from filtered nodes
    let traversed = if let Some(ref traverse) = query.traverse {
        apply_traversal(engine, &filtered, traverse)
    } else {
        filtered
    };

    // Step 4: If there's an aggregate clause, return aggregated results
    if let Some(ref aggregate) = query.aggregate {
        return apply_aggregation(engine, &traversed, aggregate);
    }

    // Step 5: Apply limit
    let limited = if let Some(limit) = query.limit {
        traversed.into_iter().take(limit).collect()
    } else {
        traversed
    };

    // Step 6: Project return fields
    let return_fields = query.return_fields.as_deref();
    let results: Vec<serde_json::Value> = limited
        .iter()
        .filter_map(|id| engine.get_node(id))
        .map(|node| project_node(node, return_fields))
        .collect();

    serde_json::json!(results)
}

/// Match nodes by label, type, id, status, or name substring.
fn match_nodes(engine: &MemoryEngine, match_clause: &Option<MatchClause>) -> Vec<String> {
    let clause = match match_clause {
        Some(c) => c,
        None => {
            // No match clause: return all node IDs
            return engine.all_node_ids();
        }
    };

    // If matching by exact ID, short-circuit
    if let Some(ref id) = clause.id {
        if engine.get_node(id).is_some() {
            return vec![id.clone()];
        } else {
            return vec![];
        }
    }

    let mut results: Vec<String> = Vec::new();

    // Filter by label first (most selective)
    let candidates: Vec<&MemoryNode> = if let Some(ref label) = clause.label {
        engine.get_nodes_by_label(label)
    } else {
        engine.all_nodes()
    };

    for node in candidates {
        // Filter by entity type
        if let Some(ref entity_type) = clause.entity_type {
            if let NodeLabel::Entity { entity_type: ref et, .. } = node.label {
                if et != entity_type {
                    continue;
                }
            } else {
                continue; // Not an entity node
            }
        }

        // Filter by status
        if let Some(ref status) = clause.status {
            let node_status = match &node.label {
                NodeLabel::Entity { status: s, .. } => s,
                NodeLabel::Workspace { status: s, .. } => s,
                _ => continue,
            };
            if node_status.as_str() != status.as_str() {
                continue;
            }
        }

        // Filter by name substring (case-insensitive)
        if let Some(ref name_contains) = clause.name_contains {
            if !node.name.to_lowercase().contains(&name_contains.to_lowercase()) {
                continue;
            }
        }

        results.push(node.id.clone());
    }

    results
}

/// Filter nodes by edge constraints (edge_to / edge_from).
fn apply_where_filter(
    engine: &MemoryEngine,
    candidates: Vec<String>,
    where_clause: &WhereClause,
) -> Vec<String> {
    candidates
        .into_iter()
        .filter(|node_id| {
            // Check edge_to: candidate must have an outbound edge to the specified node
            if let Some(ref edge_to) = where_clause.edge_to {
                let edges = engine.get_forward_edges(node_id);
                let has_match = edges
                    .iter()
                    .any(|e| e.to_id == edge_to.id && e.relationship == edge_to.relationship);
                if !has_match {
                    return false;
                }
            }

            // Check edge_from: candidate must have an inbound edge from the specified node
            if let Some(ref edge_from) = where_clause.edge_from {
                let edges = engine.get_reverse_edges(node_id);
                let has_match = edges
                    .iter()
                    .any(|e| e.from_id == edge_from.id && e.relationship == edge_from.relationship);
                if !has_match {
                    return false;
                }
            }

            true
        })
        .collect()
}

/// BFS traversal from seed nodes along edges of a specified relationship type.
fn apply_traversal(
    engine: &MemoryEngine,
    seed_ids: &[String],
    traverse: &TraverseClause,
) -> Vec<String> {
    let max_depth = traverse.max_depth.min(5); // Cap at 5 hops
    let mut visited: HashSet<String> = HashSet::new();
    let mut result: Vec<String> = Vec::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    // Seed the BFS
    for id in seed_ids {
        if visited.insert(id.clone()) {
            result.push(id.clone());
            queue.push_back((id.clone(), 0));
        }
    }

    while let Some((current_id, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }

        let neighbors = match traverse.direction.as_str() {
            "inbound" => {
                let edges = engine.get_reverse_edges(&current_id);
                edges
                    .iter()
                    .filter(|e| e.relationship == traverse.relationship)
                    .map(|e| e.from_id.clone())
                    .collect::<Vec<_>>()
            }
            "both" => {
                let mut ids = Vec::new();
                for e in engine.get_forward_edges(&current_id) {
                    if e.relationship == traverse.relationship {
                        ids.push(e.to_id.clone());
                    }
                }
                for e in engine.get_reverse_edges(&current_id) {
                    if e.relationship == traverse.relationship {
                        ids.push(e.from_id.clone());
                    }
                }
                ids
            }
            _ => {
                // "outbound" (default)
                let edges = engine.get_forward_edges(&current_id);
                edges
                    .iter()
                    .filter(|e| e.relationship == traverse.relationship)
                    .map(|e| e.to_id.clone())
                    .collect::<Vec<_>>()
            }
        };

        for neighbor_id in neighbors {
            if visited.insert(neighbor_id.clone()) {
                result.push(neighbor_id.clone());
                queue.push_back((neighbor_id, depth + 1));
            }
        }
    }

    result
}

/// Group and count nodes by a specified field.
fn apply_aggregation(
    engine: &MemoryEngine,
    node_ids: &[String],
    aggregate: &AggregateClause,
) -> serde_json::Value {
    let mut groups: HashMap<String, usize> = HashMap::new();

    for id in node_ids {
        if let Some(node) = engine.get_node(id) {
            let key = match aggregate.group_by.as_str() {
                "type" => match &node.label {
                    NodeLabel::Entity { entity_type, .. } => entity_type.clone(),
                    NodeLabel::Workspace { .. } => "Workspace".to_string(),
                    NodeLabel::Scratchpad { .. } => "Scratchpad".to_string(),
                },
                "label" => match &node.label {
                    NodeLabel::Entity { .. } => "Entity".to_string(),
                    NodeLabel::Workspace { .. } => "Workspace".to_string(),
                    NodeLabel::Scratchpad { .. } => "Scratchpad".to_string(),
                },
                "status" => match &node.label {
                    NodeLabel::Entity { status, .. } => status.clone(),
                    NodeLabel::Workspace { status, .. } => status.clone(),
                    _ => "unknown".to_string(),
                },
                _ => "unknown".to_string(),
            };

            *groups.entry(key).or_insert(0) += 1;
        }
    }

    // Sort by count descending
    let mut sorted: Vec<_> = groups.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    let results: Vec<serde_json::Value> = sorted
        .into_iter()
        .map(|(key, count)| {
            serde_json::json!({
                aggregate.group_by.clone(): key,
                "count": count,
            })
        })
        .collect();

    serde_json::json!(results)
}

/// Project a node into a JSON object, optionally filtering to specific fields.
fn project_node(node: &MemoryNode, return_fields: Option<&[String]>) -> serde_json::Value {
    let full = serde_json::json!({
        "id": node.id,
        "name": node.name,
        "label": match &node.label {
            NodeLabel::Entity { entity_type, status, last_modified } => serde_json::json!({
                "label": "Entity",
                "type": entity_type,
                "status": status,
                "last_modified": last_modified,
            }),
            NodeLabel::Workspace { description, status, closed_at } => serde_json::json!({
                "label": "Workspace",
                "description": description,
                "status": status,
                "closed_at": closed_at,
            }),
            NodeLabel::Scratchpad { created_at } => serde_json::json!({
                "label": "Scratchpad",
                "created_at": created_at,
            }),
        },
        "content": node.content,
        "metadata": node.metadata,
    });

    match return_fields {
        Some(fields) => {
            let obj = full.as_object().unwrap();
            let mut projected = serde_json::Map::new();
            for field in fields {
                if let Some(val) = obj.get(field) {
                    projected.insert(field.clone(), val.clone());
                }
            }
            serde_json::Value::Object(projected)
        }
        None => full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::MemoryEngine;
    use crate::types::*;
    use std::collections::HashMap;

    fn make_engine() -> MemoryEngine {
        let mut engine = MemoryEngine::default();

        // Create a file entity
        let mut file_props = HashMap::new();
        file_props.insert("type".into(), serde_json::json!("File"));
        file_props.insert("status".into(), serde_json::json!("active"));
        file_props.insert("last_modified".into(), serde_json::json!(1000));
        file_props.insert("name".into(), serde_json::json!("auth.ts"));
        engine.upsert_node(&UpsertNodePayload {
            id: "src/auth.ts".into(),
            label: "Entity".into(),
            properties: file_props,
        });

        // Create two function entities
        for (id, name) in [("src/auth.ts::validate", "validate"), ("src/auth.ts::parse", "parse")] {
            let mut props = HashMap::new();
            props.insert("type".into(), serde_json::json!("Function"));
            props.insert("status".into(), serde_json::json!("active"));
            props.insert("last_modified".into(), serde_json::json!(1000));
            props.insert("name".into(), serde_json::json!(name));
            engine.upsert_node(&UpsertNodePayload {
                id: id.into(),
                label: "Entity".into(),
                properties: props,
            });
        }

        // DECLARED_IN edges
        for func_id in ["src/auth.ts::validate", "src/auth.ts::parse"] {
            engine.link_nodes(&LinkNodesPayload {
                from_id: func_id.into(),
                to_id: "src/auth.ts".into(),
                relationship: "DECLARED_IN".into(),
                properties: HashMap::new(),
            });
        }

        // CALLS edge
        engine.link_nodes(&LinkNodesPayload {
            from_id: "src/auth.ts::validate".into(),
            to_id: "src/auth.ts::parse".into(),
            relationship: "CALLS".into(),
            properties: HashMap::new(),
        });

        engine
    }

    #[test]
    fn test_match_all_entities() {
        let engine = make_engine();
        let query = DslQuery {
            match_clause: Some(MatchClause {
                label: Some("Entity".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = evaluate_query(&engine, &query);
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn test_match_by_type() {
        let engine = make_engine();
        let query = DslQuery {
            match_clause: Some(MatchClause {
                label: Some("Entity".into()),
                entity_type: Some("Function".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = evaluate_query(&engine, &query);
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn test_where_edge_to() {
        let engine = make_engine();
        let query = DslQuery {
            match_clause: Some(MatchClause {
                label: Some("Entity".into()),
                entity_type: Some("Function".into()),
                ..Default::default()
            }),
            where_clause: Some(WhereClause {
                edge_to: Some(EdgeFilter {
                    id: "src/auth.ts".into(),
                    relationship: "DECLARED_IN".into(),
                }),
                edge_from: None,
            }),
            ..Default::default()
        };
        let result = evaluate_query(&engine, &query);
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2); // Both functions are DECLARED_IN auth.ts
    }

    #[test]
    fn test_traversal() {
        let engine = make_engine();
        let query = DslQuery {
            match_clause: Some(MatchClause {
                id: Some("src/auth.ts::validate".into()),
                ..Default::default()
            }),
            traverse: Some(TraverseClause {
                relationship: "CALLS".into(),
                direction: "outbound".into(),
                max_depth: 2,
            }),
            ..Default::default()
        };
        let result = evaluate_query(&engine, &query);
        let arr = result.as_array().unwrap();
        // validate + parse (via CALLS)
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn test_aggregation() {
        let engine = make_engine();
        let query = DslQuery {
            match_clause: Some(MatchClause {
                label: Some("Entity".into()),
                ..Default::default()
            }),
            aggregate: Some(AggregateClause {
                group_by: "type".into(),
                count: true,
            }),
            ..Default::default()
        };
        let result = evaluate_query(&engine, &query);
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2); // Function and File
    }

    #[test]
    fn test_return_fields_projection() {
        let engine = make_engine();
        let query = DslQuery {
            match_clause: Some(MatchClause {
                id: Some("src/auth.ts".into()),
                ..Default::default()
            }),
            return_fields: Some(vec!["id".into(), "name".into()]),
            ..Default::default()
        };
        let result = evaluate_query(&engine, &query);
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let obj = arr[0].as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert!(obj.contains_key("id"));
        assert!(obj.contains_key("name"));
    }
}
