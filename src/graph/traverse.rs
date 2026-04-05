//! Graph traversal — recursive depth-first with cycle detection.

use surrealdb::Surreal;

use super::confidence;
use super::error::GraphError;
use super::store::Db;
use super::types::*;

/// Traverse the graph from a named entity up to a given depth.
/// Skips superseded relationships (valid_until IS NOT NULL).
pub async fn traverse(
    db: &Surreal<Db>,
    entity_name: &str,
    max_depth: u32,
) -> Result<TraversalNode, GraphError> {
    traverse_filtered(db, entity_name, max_depth, None).await
}

/// Traverse with an optional entity type filter.
/// When `type_filter` is set, only neighbors matching that type are expanded.
pub async fn traverse_filtered(
    db: &Surreal<Db>,
    entity_name: &str,
    max_depth: u32,
    type_filter: Option<&str>,
) -> Result<TraversalNode, GraphError> {
    // Load root as full entity (for access_count increment), project to L0
    let full = super::crud::get_entity_by_name(db, entity_name)
        .await?
        .ok_or_else(|| GraphError::NotFound(entity_name.to_string()))?;

    // Increment access count on root entity only
    super::crud::increment_access_counts(db, &[full.id_string()]).await?;

    let root = EntitySummary {
        id: full.id.clone(),
        name: full.name,
        entity_type: full.entity_type,
        abstract_text: full.abstract_text,
    };

    traverse_from(db, &root, max_depth, 0, &mut vec![], type_filter).await
}

type TraversalFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<TraversalNode, GraphError>> + 'a>>;

/// Recursive traversal with cycle detection, using L0 projections.
fn traverse_from<'a>(
    db: &'a Surreal<Db>,
    entity: &'a EntitySummary,
    max_depth: u32,
    current_depth: u32,
    visited: &'a mut Vec<String>,
    type_filter: Option<&'a str>,
) -> TraversalFuture<'a> {
    Box::pin(async move {
        visited.push(entity.id_string());

        if current_depth >= max_depth {
            return Ok(TraversalNode {
                entity: entity.clone(),
                edges: vec![],
            });
        }

        let mut edges = Vec::new();

        let now = chrono::Utc::now();

        // Get outgoing relationships that are still active
        let mut response = db
            .query(
                r#"
            SELECT
                rel_type,
                valid_from,
                valid_until,
                confidence,
                last_reinforced,
                out AS target_id
            FROM relates_to
            WHERE in = type::record($id)
              AND valid_until IS NONE
            "#,
            )
            .bind(("id", entity.id_string()))
            .await?;

        let outgoing: Vec<EdgeRow> = super::deserialize_take(&mut response, 0)?;
        collect_edges(
            db,
            outgoing,
            "->",
            max_depth,
            current_depth,
            visited,
            type_filter,
            &mut edges,
            &now,
        )
        .await?;

        // Get incoming relationships
        let mut response = db
            .query(
                r#"
            SELECT
                rel_type,
                valid_from,
                valid_until,
                confidence,
                last_reinforced,
                in AS target_id
            FROM relates_to
            WHERE out = type::record($id)
              AND valid_until IS NONE
            "#,
            )
            .bind(("id", entity.id_string()))
            .await?;

        let incoming: Vec<EdgeRow> = super::deserialize_take(&mut response, 0)?;
        collect_edges(
            db,
            incoming,
            "<-",
            max_depth,
            current_depth,
            visited,
            type_filter,
            &mut edges,
            &now,
        )
        .await?;

        Ok(TraversalNode {
            entity: entity.clone(),
            edges,
        })
    })
}

/// Process edge rows, load targets as L0, apply type filter and decay, recurse.
#[allow(clippy::too_many_arguments)]
async fn collect_edges<'a>(
    db: &'a Surreal<Db>,
    edge_rows: Vec<EdgeRow>,
    direction: &str,
    max_depth: u32,
    current_depth: u32,
    visited: &'a mut Vec<String>,
    type_filter: Option<&'a str>,
    edges: &'a mut Vec<TraversalEdge>,
    now: &'a chrono::DateTime<chrono::Utc>,
) -> Result<(), GraphError> {
    for edge in edge_rows {
        // Apply temporal decay at read time
        let effective = confidence::effective_confidence(
            edge.confidence,
            edge.last_reinforced.as_ref(),
            &edge.valid_from,
            now,
        );

        // Filter by effective confidence (not stored)
        if effective < 0.1 {
            continue;
        }

        let tid = edge.target_id_string();

        // Load L0 projection
        let target: Option<EntitySummary> = super::crud::get_entity_summary(db, &tid).await?;
        if let Some(target) = target {
            if visited.contains(&target.id_string()) {
                continue;
            }

            // Apply type filter
            if let Some(filter) = type_filter {
                if target.entity_type.to_string() != filter {
                    continue;
                }
            }

            let child = traverse_from(
                db,
                &target,
                max_depth,
                current_depth + 1,
                visited,
                type_filter,
            )
            .await?;

            edges.push(TraversalEdge {
                rel_type: edge.rel_type,
                direction: direction.to_string(),
                target: child,
                valid_from: edge.valid_from,
                valid_until: edge.valid_until,
                confidence: effective,
            });
        }
    }

    Ok(())
}

/// Format a traversal tree as an indented string for display.
pub fn format_traversal(node: &TraversalNode, indent: usize) -> String {
    let mut out = String::new();
    let prefix = "  ".repeat(indent);

    if indent == 0 {
        out.push_str(&format!(
            "{} ({})\n",
            node.entity.name, node.entity.entity_type
        ));
    }

    for edge in &node.edges {
        let superseded = if edge.valid_until.is_some() {
            " [superseded]"
        } else {
            ""
        };

        let confidence_tag = if edge.confidence < 1.0 {
            format!(" [{}%]", (edge.confidence * 100.0).round() as u32)
        } else {
            String::new()
        };

        out.push_str(&format!(
            "{}{} {} {} {}{}{}\n",
            prefix,
            "├──",
            edge.direction,
            edge.rel_type,
            edge.target.entity.name,
            confidence_tag,
            superseded,
        ));

        if !edge.target.edges.is_empty() {
            out.push_str(&format_traversal(&edge.target, indent + 1));
        }
    }

    out
}
