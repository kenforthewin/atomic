use super::PostgresStorage;
use crate::clustering;
use crate::error::AtomicCoreError;
use crate::models::*;
use crate::storage::traits::*;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

/// Maximum nodes to show before aggregating into semantic clusters
const MAX_TAGS_PER_LEVEL: usize = 40;
/// Maximum atoms to show before sub-clustering
const MAX_ATOMS_PER_LEVEL: usize = 50;
/// Top N tags to show individually when aggregating
const TOP_TAGS_SHOWN: usize = 20;
/// Minimum similarity for semantic edges used in clustering
const CLUSTER_MIN_SIMILARITY: f32 = 0.5;
/// Maximum number of atoms for edge computation
const MAX_ATOMS_FOR_EDGES: usize = 2000;
/// Maximum tags to attempt semantic clustering on
const MAX_TAGS_FOR_CLUSTERING: usize = 200;
/// Target number of groups when doing count-based grouping
const COUNT_GROUP_TARGET: usize = 15;

// ==================== Tag Tree ====================

/// Precomputed tag tree data — loaded once per request, reused across functions.
struct TagTree {
    /// (id, name, parent_id) for every tag
    #[allow(dead_code)]
    all_tags: Vec<(String, String, Option<String>)>,
    /// tag_id -> direct atom count
    direct_counts: HashMap<String, i32>,
    /// tag_id -> transitive atom count (including descendants)
    transitive_counts: HashMap<String, i32>,
    /// tag_id -> child tag IDs
    children_map: HashMap<String, Vec<String>>,
    /// tag_id -> tag name
    tag_names: HashMap<String, String>,
}

impl TagTree {
    async fn load(pool: &sqlx::PgPool, db_id: &str) -> Result<Self, AtomicCoreError> {
        let all_tags: Vec<(String, String, Option<String>)> =
            sqlx::query_as("SELECT id, name, parent_id FROM tags WHERE db_id = $1 ORDER BY name")
                .bind(db_id)
                .fetch_all(pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let direct_count_rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT tag_id, COUNT(*) FROM atom_tags WHERE db_id = $1 GROUP BY tag_id",
        )
        .bind(db_id)
        .fetch_all(pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let direct_counts: HashMap<String, i32> = direct_count_rows
            .into_iter()
            .map(|(id, count)| (id, count as i32))
            .collect();

        let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
        for (id, _, parent) in &all_tags {
            if let Some(p) = parent {
                children_map.entry(p.clone()).or_default().push(id.clone());
            }
        }

        let tag_names: HashMap<String, String> = all_tags
            .iter()
            .map(|(id, name, _)| (id.clone(), name.clone()))
            .collect();

        let mut transitive_counts: HashMap<String, i32> = HashMap::new();
        for (id, _, _) in &all_tags {
            compute_transitive_cached(id, &children_map, &direct_counts, &mut transitive_counts);
        }

        Ok(TagTree {
            all_tags,
            direct_counts,
            transitive_counts,
            children_map,
            tag_names,
        })
    }

    fn has_children(&self, tag_id: &str) -> bool {
        self.children_map
            .get(tag_id)
            .map_or(false, |c| !c.is_empty())
    }

    fn transitive_count(&self, tag_id: &str) -> i32 {
        self.transitive_counts.get(tag_id).copied().unwrap_or(0)
    }

    fn name(&self, tag_id: &str) -> String {
        self.tag_names
            .get(tag_id)
            .cloned()
            .unwrap_or_else(|| tag_id.to_string())
    }

    fn descendant_tag_ids(&self, tag_id: &str) -> Vec<String> {
        let mut result = vec![tag_id.to_string()];
        let mut stack = vec![tag_id.to_string()];
        while let Some(tid) = stack.pop() {
            if let Some(kids) = self.children_map.get(&tid) {
                for kid in kids {
                    result.push(kid.clone());
                    stack.push(kid.clone());
                }
            }
        }
        result
    }
}

fn compute_transitive_cached(
    tag_id: &str,
    children_map: &HashMap<String, Vec<String>>,
    direct_counts: &HashMap<String, i32>,
    cache: &mut HashMap<String, i32>,
) -> i32 {
    if let Some(&cached) = cache.get(tag_id) {
        return cached;
    }
    let own = direct_counts.get(tag_id).copied().unwrap_or(0);
    let child_sum: i32 = children_map
        .get(tag_id)
        .map(|kids| {
            kids.iter()
                .map(|kid| compute_transitive_cached(kid, children_map, direct_counts, cache))
                .sum()
        })
        .unwrap_or(0);
    let total = own + child_sum;
    cache.insert(tag_id.to_string(), total);
    total
}

// ==================== Helper: get dominant tags for atoms ====================

async fn get_dominant_tags_for_atoms(
    pool: &sqlx::PgPool,
    atom_ids: &[String],
    db_id: &str,
) -> Result<Vec<String>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(vec![]);
    }
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT t.name
         FROM atom_tags at
         JOIN tags t ON at.tag_id = t.id
         WHERE at.atom_id = ANY($1)
         AND t.parent_id IS NOT NULL
         AND at.db_id = $2 AND t.db_id = $2
         GROUP BY t.id, t.name
         ORDER BY COUNT(*) DESC
         LIMIT 3",
    )
    .bind(atom_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    Ok(rows.into_iter().map(|(name,)| name).collect())
}

async fn get_dominant_tags_for_cluster(
    pool: &sqlx::PgPool,
    atom_ids: &[String],
    db_id: &str,
) -> Result<Vec<String>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(vec![]);
    }
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT t.name
         FROM atom_tags at
         JOIN tags t ON at.tag_id = t.id
         WHERE at.atom_id = ANY($1)
         AND at.db_id = $2 AND t.db_id = $2
         GROUP BY t.id, t.name
         ORDER BY COUNT(*) DESC
         LIMIT 3",
    )
    .bind(atom_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    Ok(rows.into_iter().map(|(name,)| name).collect())
}

// ==================== Helper: load semantic edges ====================

async fn load_semantic_edges_for_atoms(
    pool: &sqlx::PgPool,
    atom_ids: &[String],
    db_id: &str,
) -> Result<Vec<(String, String, f32)>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(vec![]);
    }
    let rows: Vec<(String, String, f32)> = sqlx::query_as(
        "SELECT source_atom_id, target_atom_id, similarity_score
         FROM semantic_edges
         WHERE source_atom_id = ANY($1) AND target_atom_id = ANY($1)
         AND similarity_score >= $2 AND db_id = $3",
    )
    .bind(atom_ids)
    .bind(CLUSTER_MIN_SIMILARITY)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    Ok(rows)
}

// ==================== Helper: snippet label ====================

fn snippet_label(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.len() > 60 {
        // Find a valid char boundary at or before byte 57
        let mut end = 57;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &trimmed[..end])
    } else {
        trimmed.to_string()
    }
}

// ==================== Helper: build flat atom nodes ====================

async fn build_flat_atom_nodes(
    pool: &sqlx::PgPool,
    atom_ids: &[String],
    db_id: &str,
) -> Result<Vec<CanvasNode>, AtomicCoreError> {
    if atom_ids.is_empty() {
        return Ok(vec![]);
    }

    let snippet_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, SUBSTRING(content FROM 1 FOR 100) FROM atoms WHERE id = ANY($1) AND db_id = $2",
    )
    .bind(atom_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let snippets: HashMap<String, String> = snippet_rows.into_iter().collect();

    let tag_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT at.atom_id, t.name FROM atom_tags at
         JOIN tags t ON at.tag_id = t.id
         WHERE at.atom_id = ANY($1) AND at.db_id = $2 AND t.db_id = $2
         ORDER BY t.name",
    )
    .bind(atom_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut atom_tags: HashMap<String, Vec<String>> = HashMap::new();
    for (atom_id, tag_name) in tag_rows {
        atom_tags.entry(atom_id).or_default().push(tag_name);
    }

    Ok(snippets
        .into_iter()
        .map(|(id, content)| {
            let tags = atom_tags.get(&id).cloned().unwrap_or_default();
            CanvasNode {
                id,
                node_type: CanvasNodeType::Atom,
                label: snippet_label(&content),
                atom_count: 1,
                children_ids: vec![],
                dominant_tags: tags,
                centroid: None,
            }
        })
        .collect())
}

// ==================== Helper: compute edges between atom sets ====================

async fn compute_edges_for_atom_set(
    pool: &sqlx::PgPool,
    atom_ids: &[String],
    db_id: &str,
) -> Result<Vec<CanvasEdge>, AtomicCoreError> {
    if atom_ids.len() <= 1 {
        return Ok(vec![]);
    }

    let edges = load_semantic_edges_for_atoms(pool, atom_ids, db_id).await?;

    Ok(edges
        .into_iter()
        .filter(|(_, _, score)| *score >= 0.2)
        .map(|(source, target, weight)| CanvasEdge {
            source_id: source,
            target_id: target,
            weight,
        })
        .collect())
}

async fn compute_edges_between_nodes_simple(
    pool: &sqlx::PgPool,
    nodes: &[CanvasNode],
    db_id: &str,
) -> Result<Vec<CanvasEdge>, AtomicCoreError> {
    if nodes.len() <= 1 {
        return Ok(vec![]);
    }

    // For simplicity, use children_ids from each node
    let mut node_atoms: HashMap<String, Vec<String>> = HashMap::new();
    for node in nodes {
        if !node.children_ids.is_empty() {
            node_atoms.insert(node.id.clone(), node.children_ids.clone());
        } else if node.node_type == CanvasNodeType::Atom {
            node_atoms.insert(node.id.clone(), vec![node.id.clone()]);
        }
    }

    let mut all_atom_ids: Vec<String> = node_atoms.values().flatten().cloned().collect();
    all_atom_ids.sort();
    all_atom_ids.dedup();

    if all_atom_ids.is_empty() || all_atom_ids.len() > MAX_ATOMS_FOR_EDGES {
        return Ok(vec![]);
    }

    let edges = load_semantic_edges_for_atoms(pool, &all_atom_ids, db_id).await?;

    // Build atom->node mapping
    let mut atom_to_node: HashMap<String, String> = HashMap::new();
    for (node_id, atoms) in &node_atoms {
        for atom_id in atoms {
            atom_to_node.insert(atom_id.clone(), node_id.clone());
        }
    }

    // Aggregate atom-level edges to node-level
    let mut pair_scores: HashMap<(String, String), (f32, i32)> = HashMap::new();
    for (src, tgt, score) in &edges {
        let src_node = atom_to_node.get(src);
        let tgt_node = atom_to_node.get(tgt);
        if let (Some(sn), Some(tn)) = (src_node, tgt_node) {
            if sn != tn {
                let key = if sn < tn {
                    (sn.clone(), tn.clone())
                } else {
                    (tn.clone(), sn.clone())
                };
                let entry = pair_scores.entry(key).or_insert((0.0, 0));
                entry.0 += score;
                entry.1 += 1;
            }
        }
    }

    Ok(pair_scores
        .into_iter()
        .map(|((src, tgt), (total, count))| CanvasEdge {
            source_id: src,
            target_id: tgt,
            weight: total / count as f32,
        })
        .filter(|e| e.weight >= 0.2)
        .collect())
}

async fn compute_edges_if_small(
    pool: &sqlx::PgPool,
    nodes: &[CanvasNode],
    db_id: &str,
) -> Result<Vec<CanvasEdge>, AtomicCoreError> {
    if nodes.len() <= 1 {
        return Ok(vec![]);
    }
    let total_atoms: i64 = nodes.iter().map(|n| n.atom_count as i64).sum();
    if total_atoms > MAX_ATOMS_FOR_EDGES as i64 {
        return Ok(vec![]);
    }
    compute_edges_between_nodes_simple(pool, nodes, db_id).await
}

// ==================== Helper: cluster atoms into groups ====================

async fn cluster_atoms_into_groups(
    pool: &sqlx::PgPool,
    atom_ids: &[String],
    parent_id: &str,
    db_id: &str,
) -> Result<Vec<CanvasNode>, AtomicCoreError> {
    let edges = load_semantic_edges_for_atoms(pool, atom_ids, db_id).await?;

    if edges.is_empty() {
        return build_flat_atom_nodes(pool, atom_ids, db_id).await;
    }

    let labels = clustering::label_propagation(&edges);
    let groups = clustering::group_labels_into_clusters(&labels, 2);

    if groups.len() <= 1 {
        return build_flat_atom_nodes(pool, atom_ids, db_id).await;
    }

    let clustered: HashSet<&String> = labels.keys().collect();
    let unclustered: Vec<String> = atom_ids
        .iter()
        .filter(|id| !clustered.contains(id))
        .cloned()
        .collect();

    let mut nodes = Vec::new();

    for (i, group) in groups.iter().enumerate() {
        if group.len() <= 3 {
            let mut atom_nodes = build_flat_atom_nodes(pool, group, db_id).await?;
            nodes.append(&mut atom_nodes);
        } else {
            let dominant = get_dominant_tags_for_atoms(pool, group, db_id).await?;
            let label = if dominant.len() >= 2 {
                format!("{}, {}", dominant[0], dominant[1])
            } else if !dominant.is_empty() {
                dominant[0].clone()
            } else {
                format!("Group {}", i + 1)
            };

            nodes.push(CanvasNode {
                id: format!("cluster:{}:{}", parent_id, i),
                node_type: CanvasNodeType::SemanticCluster,
                label,
                atom_count: group.len() as i32,
                children_ids: group.clone(),
                dominant_tags: dominant,
                centroid: None,
            });
        }
    }

    if !unclustered.is_empty() {
        let limit = MAX_ATOMS_PER_LEVEL
            .saturating_sub(nodes.len())
            .min(unclustered.len());
        let mut atom_nodes = build_flat_atom_nodes(pool, &unclustered[..limit], db_id).await?;
        nodes.append(&mut atom_nodes);

        if unclustered.len() > limit {
            let remaining = &unclustered[limit..];
            let dominant = get_dominant_tags_for_atoms(pool, remaining, db_id).await?;
            nodes.push(CanvasNode {
                id: format!("cluster:{}:unclustered", parent_id),
                node_type: CanvasNodeType::SemanticCluster,
                label: "Other".to_string(),
                atom_count: remaining.len() as i32,
                children_ids: remaining.to_vec(),
                dominant_tags: dominant,
                centroid: None,
            });
        }
    }

    Ok(nodes)
}

// ==================== Helper: cluster tags by similarity ====================

async fn cluster_tags_by_similarity(
    pool: &sqlx::PgPool,
    tag_ids: &[String],
    tree: &TagTree,
    parent_id: &str,
    db_id: &str,
) -> Result<Vec<CanvasNode>, AtomicCoreError> {
    // Expand each tag to descendants, batch resolve atom IDs
    let mut descendant_to_original: HashMap<String, String> = HashMap::new();
    for tid in tag_ids {
        for desc in tree.descendant_tag_ids(tid) {
            descendant_to_original
                .entry(desc)
                .or_insert_with(|| tid.clone());
        }
    }

    let all_desc_ids: Vec<String> = descendant_to_original.keys().cloned().collect();

    let at_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT tag_id, atom_id FROM atom_tags WHERE tag_id = ANY($1) AND db_id = $2",
    )
    .bind(&all_desc_ids)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut tag_to_atoms: HashMap<String, Vec<String>> = HashMap::new();
    for tid in tag_ids {
        tag_to_atoms.insert(tid.clone(), Vec::new());
    }
    for (desc_tag, atom_id) in &at_rows {
        if let Some(original) = descendant_to_original.get(desc_tag) {
            tag_to_atoms
                .entry(original.clone())
                .or_default()
                .push(atom_id.clone());
        }
    }

    let mut all_atom_ids: Vec<String> = tag_to_atoms.values().flatten().cloned().collect();
    all_atom_ids.sort();
    all_atom_ids.dedup();

    let edges = load_semantic_edges_for_atoms(pool, &all_atom_ids, db_id).await?;

    // Build atom-to-tag mapping
    let mut atom_to_tag: HashMap<String, String> = HashMap::new();
    for (tid, atoms) in &tag_to_atoms {
        for aid in atoms {
            atom_to_tag.insert(aid.clone(), tid.clone());
        }
    }

    // Convert atom-level edges to tag-level
    let mut tag_edge_counts: HashMap<(String, String), (f32, i32)> = HashMap::new();
    for (src, tgt, score) in &edges {
        let src_tag = atom_to_tag.get(src);
        let tgt_tag = atom_to_tag.get(tgt);
        if let (Some(st), Some(tt)) = (src_tag, tgt_tag) {
            if st != tt {
                let key = if st < tt {
                    (st.clone(), tt.clone())
                } else {
                    (tt.clone(), st.clone())
                };
                let entry = tag_edge_counts.entry(key).or_insert((0.0, 0));
                entry.0 += score;
                entry.1 += 1;
            }
        }
    }

    let tag_edges: Vec<(String, String, f32)> = tag_edge_counts
        .into_iter()
        .map(|((a, b), (total_score, count))| (a, b, total_score / count as f32))
        .collect();

    let labels = clustering::label_propagation(&tag_edges);
    let groups = clustering::group_labels_into_clusters(&labels, 1);

    let clustered_ids: HashSet<&String> = labels.keys().collect();
    let mut extra_groups: Vec<Vec<String>> = tag_ids
        .iter()
        .filter(|id| !clustered_ids.contains(id))
        .map(|id| vec![id.clone()])
        .collect();

    let mut all_groups = groups;
    all_groups.append(&mut extra_groups);

    let mut nodes = Vec::new();
    for (i, group) in all_groups.iter().enumerate() {
        if group.len() == 1 {
            let tid = &group[0];
            let count = tree.transitive_count(tid);
            nodes.push(CanvasNode {
                id: tid.clone(),
                node_type: CanvasNodeType::Tag,
                label: tree.name(tid),
                atom_count: count,
                children_ids: vec![],
                dominant_tags: vec![],
                centroid: None,
            });
        } else {
            let total_count: i32 = group.iter().map(|tid| tree.transitive_count(tid)).sum();
            let mut tag_counts: Vec<(&String, i32)> = group
                .iter()
                .map(|tid| (tid, tree.transitive_count(tid)))
                .collect();
            tag_counts.sort_by(|a, b| b.1.cmp(&a.1));
            let dominant: Vec<String> = tag_counts
                .iter()
                .take(2)
                .map(|(tid, _)| tree.name(tid))
                .collect();

            let label = if dominant.len() >= 2 {
                format!("{}, {} +{}", dominant[0], dominant[1], group.len() - 2)
            } else if !dominant.is_empty() {
                format!("{} +{}", dominant[0], group.len() - 1)
            } else {
                format!("Cluster {}", i + 1)
            };

            nodes.push(CanvasNode {
                id: format!("cluster:{}:{}", parent_id, i),
                node_type: CanvasNodeType::SemanticCluster,
                label,
                atom_count: total_count,
                children_ids: group.clone(),
                dominant_tags: dominant,
                centroid: None,
            });
        }
    }

    Ok(nodes)
}

/// Fast count-based grouping for very large tag sets.
fn group_tags_by_count(
    sorted_tags: &[(CanvasNode, i32)],
    tree: &TagTree,
    parent_id: &str,
) -> Vec<CanvasNode> {
    if sorted_tags.is_empty() {
        return vec![];
    }

    let group_size = (sorted_tags.len() + COUNT_GROUP_TARGET - 1) / COUNT_GROUP_TARGET;

    sorted_tags
        .chunks(group_size)
        .enumerate()
        .map(|(i, chunk)| {
            let total_count: i32 = chunk.iter().map(|(_, c)| *c).sum();
            let children_ids: Vec<String> = chunk.iter().map(|(n, _)| n.id.clone()).collect();
            let dominant: Vec<String> = chunk
                .iter()
                .take(2)
                .map(|(n, _)| tree.name(&n.id))
                .collect();

            let label = if dominant.len() >= 2 {
                format!("{}, {} +{}", dominant[0], dominant[1], chunk.len() - 2)
            } else if !dominant.is_empty() {
                format!("{} +{}", dominant[0], chunk.len() - 1)
            } else {
                format!("Group {}", i + 1)
            };

            CanvasNode {
                id: format!("cluster:{}:{}", parent_id, i),
                node_type: CanvasNodeType::SemanticCluster,
                label,
                atom_count: total_count,
                children_ids,
                dominant_tags: dominant,
                centroid: None,
            }
        })
        .collect()
}

// ==================== Breadcrumb ====================

async fn build_breadcrumb(
    pool: &sqlx::PgPool,
    tag_id: &str,
    db_id: &str,
) -> Result<Vec<BreadcrumbEntry>, AtomicCoreError> {
    let mut path = Vec::new();
    let mut current_id = Some(tag_id.to_string());

    while let Some(id) = current_id {
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT name, parent_id FROM tags WHERE id = $1 AND db_id = $2")
                .bind(&id)
                .bind(db_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        match row {
            Some((name, parent_id)) => {
                path.push(BreadcrumbEntry {
                    id: id.clone(),
                    label: name,
                });
                current_id = parent_id;
            }
            None => break,
        }
    }

    path.reverse();
    Ok(path)
}

// ==================== Canvas Level Builders ====================

async fn build_root_level(
    pool: &sqlx::PgPool,
    db_id: &str,
) -> Result<CanvasLevel, AtomicCoreError> {
    // Load semantic edges and compute clusters
    let edge_rows: Vec<(String, String, f32)> = sqlx::query_as(
        "SELECT source_atom_id, target_atom_id, similarity_score
         FROM semantic_edges
         WHERE similarity_score >= $1 AND db_id = $2",
    )
    .bind(CLUSTER_MIN_SIMILARITY)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let labels = clustering::label_propagation(&edge_rows);
    let groups = clustering::group_labels_into_clusters(&labels, 3);

    let mut nodes: Vec<CanvasNode> = Vec::new();
    let mut clustered_atom_ids: HashSet<String> = HashSet::new();

    for (i, group) in groups.iter().enumerate() {
        for aid in group {
            clustered_atom_ids.insert(aid.clone());
        }

        let dominant = get_dominant_tags_for_cluster(pool, group, db_id)
            .await
            .unwrap_or_default();

        let label = if dominant.len() >= 2 {
            format!("{}, {}", dominant[0], dominant[1])
        } else if !dominant.is_empty() {
            dominant[0].clone()
        } else {
            format!("Cluster {}", i + 1)
        };

        nodes.push(CanvasNode {
            id: format!("cluster:{}", i),
            node_type: CanvasNodeType::SemanticCluster,
            label,
            atom_count: group.len() as i32,
            children_ids: group.clone(),
            dominant_tags: dominant,
            centroid: None,
        });
    }

    // Find unclustered atoms
    let all_atom_ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM atoms WHERE db_id = $1 ORDER BY updated_at DESC")
            .bind(db_id)
            .fetch_all(pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let unclustered_ids: Vec<String> = all_atom_ids
        .into_iter()
        .filter(|id| !clustered_atom_ids.contains(id))
        .collect();

    if !unclustered_ids.is_empty() {
        if unclustered_ids.len() <= MAX_ATOMS_PER_LEVEL {
            let mut atom_nodes = build_flat_atom_nodes(pool, &unclustered_ids, db_id).await?;
            nodes.append(&mut atom_nodes);
        } else {
            let dominant = get_dominant_tags_for_atoms(pool, &unclustered_ids, db_id)
                .await
                .unwrap_or_default();
            nodes.push(CanvasNode {
                id: "cluster:unclustered".to_string(),
                node_type: CanvasNodeType::SemanticCluster,
                label: "Unclustered".to_string(),
                atom_count: unclustered_ids.len() as i32,
                children_ids: unclustered_ids,
                dominant_tags: dominant,
                centroid: None,
            });
        }
    }

    let edges = compute_edges_between_nodes_simple(pool, &nodes, db_id).await?;

    Ok(CanvasLevel {
        parent_id: None,
        parent_label: None,
        breadcrumb: vec![],
        nodes,
        edges,
    })
}

async fn build_tag_level(
    pool: &sqlx::PgPool,
    tag_id: &str,
    db_id: &str,
) -> Result<CanvasLevel, AtomicCoreError> {
    if tag_id == "untagged" {
        return build_untagged_level(pool, db_id).await;
    }

    let tree = TagTree::load(pool, db_id).await?;

    let (parent_name, _parent_parent_id): (String, Option<String>) =
        sqlx::query_as("SELECT name, parent_id FROM tags WHERE id = $1 AND db_id = $2")
            .bind(tag_id)
            .bind(db_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            .ok_or_else(|| AtomicCoreError::NotFound(format!("Tag {} not found", tag_id)))?;

    let breadcrumb = build_breadcrumb(pool, tag_id, db_id).await?;

    let child_ids: Vec<String> = tree.children_map.get(tag_id).cloned().unwrap_or_default();

    if !child_ids.is_empty() {
        let mut tag_nodes: Vec<(CanvasNode, i32)> = child_ids
            .iter()
            .map(|id| {
                let count = tree.transitive_count(id);
                let node_type = if tree.has_children(id) {
                    CanvasNodeType::Category
                } else {
                    CanvasNodeType::Tag
                };
                (
                    CanvasNode {
                        id: id.clone(),
                        node_type,
                        label: tree.name(id),
                        atom_count: count,
                        children_ids: vec![],
                        dominant_tags: vec![],
                        centroid: None,
                    },
                    count,
                )
            })
            .filter(|(_, count)| *count > 0)
            .collect();

        tag_nodes.sort_by(|a, b| b.1.cmp(&a.1));

        let mut nodes: Vec<CanvasNode>;

        if tag_nodes.len() <= MAX_TAGS_PER_LEVEL {
            nodes = tag_nodes.into_iter().map(|(n, _)| n).collect();
        } else {
            let (top, rest) = tag_nodes.split_at(TOP_TAGS_SHOWN);
            nodes = top.iter().map(|(n, _)| n.clone()).collect();

            if rest.len() <= MAX_TAGS_FOR_CLUSTERING {
                let rest_ids: Vec<String> = rest.iter().map(|(n, _)| n.id.clone()).collect();
                let cluster_nodes =
                    cluster_tags_by_similarity(pool, &rest_ids, &tree, tag_id, db_id).await?;
                nodes.extend(cluster_nodes);
            } else {
                let group_nodes = group_tags_by_count(rest, &tree, tag_id);
                nodes.extend(group_nodes);
            }
        }

        // Also add atoms directly tagged with this tag
        let direct_atom_count = tree.direct_counts.get(tag_id).copied().unwrap_or(0);
        if direct_atom_count > 0 {
            nodes.push(CanvasNode {
                id: format!("direct:{}", tag_id),
                node_type: CanvasNodeType::Tag,
                label: format!("{} (direct)", parent_name),
                atom_count: direct_atom_count,
                children_ids: vec![],
                dominant_tags: vec![],
                centroid: None,
            });
        }

        let edges = compute_edges_if_small(pool, &nodes, db_id).await?;

        Ok(CanvasLevel {
            parent_id: Some(tag_id.to_string()),
            parent_label: Some(parent_name),
            breadcrumb,
            nodes,
            edges,
        })
    } else {
        // Leaf tag — show atoms
        build_atoms_for_tag(pool, tag_id, &parent_name, &breadcrumb, db_id).await
    }
}

async fn build_untagged_level(
    pool: &sqlx::PgPool,
    db_id: &str,
) -> Result<CanvasLevel, AtomicCoreError> {
    let breadcrumb = vec![BreadcrumbEntry {
        id: "untagged".to_string(),
        label: "Untagged".to_string(),
    }];

    let atoms: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, SUBSTRING(content FROM 1 FOR 100) FROM atoms
         WHERE id NOT IN (SELECT atom_id FROM atom_tags WHERE db_id = $1)
         AND db_id = $1
         ORDER BY updated_at DESC",
    )
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    if atoms.len() <= MAX_ATOMS_PER_LEVEL {
        let atom_ids: Vec<String> = atoms.iter().map(|(id, _)| id.clone()).collect();
        let nodes = build_flat_atom_nodes(pool, &atom_ids, db_id).await?;
        let edges = compute_edges_for_atom_set(pool, &atom_ids, db_id).await?;

        Ok(CanvasLevel {
            parent_id: Some("untagged".to_string()),
            parent_label: Some("Untagged".to_string()),
            breadcrumb,
            nodes,
            edges,
        })
    } else {
        let atom_ids: Vec<String> = atoms.iter().map(|(id, _)| id.clone()).collect();
        let nodes = cluster_atoms_into_groups(pool, &atom_ids, "untagged", db_id).await?;
        let edges = compute_edges_between_nodes_simple(pool, &nodes, db_id).await?;

        Ok(CanvasLevel {
            parent_id: Some("untagged".to_string()),
            parent_label: Some("Untagged".to_string()),
            breadcrumb,
            nodes,
            edges,
        })
    }
}

async fn build_atoms_for_tag(
    pool: &sqlx::PgPool,
    tag_id: &str,
    tag_name: &str,
    breadcrumb: &[BreadcrumbEntry],
    db_id: &str,
) -> Result<CanvasLevel, AtomicCoreError> {
    let actual_tag_id = tag_id.strip_prefix("direct:").unwrap_or(tag_id);

    let atoms: Vec<(String, String)> = sqlx::query_as(
        "SELECT a.id, SUBSTRING(a.content FROM 1 FOR 100) FROM atoms a
         INNER JOIN atom_tags at ON a.id = at.atom_id
         WHERE at.tag_id = $1 AND a.db_id = $2 AND at.db_id = $2
         ORDER BY a.updated_at DESC",
    )
    .bind(actual_tag_id)
    .bind(db_id)
    .fetch_all(pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    if atoms.len() <= MAX_ATOMS_PER_LEVEL {
        let atom_ids: Vec<String> = atoms.iter().map(|(id, _)| id.clone()).collect();
        let nodes = build_flat_atom_nodes(pool, &atom_ids, db_id).await?;
        let edges = compute_edges_for_atom_set(pool, &atom_ids, db_id).await?;

        Ok(CanvasLevel {
            parent_id: Some(tag_id.to_string()),
            parent_label: Some(tag_name.to_string()),
            breadcrumb: breadcrumb.to_vec(),
            nodes,
            edges,
        })
    } else {
        let atom_ids: Vec<String> = atoms.iter().map(|(id, _)| id.clone()).collect();
        let nodes = cluster_atoms_into_groups(pool, &atom_ids, tag_id, db_id).await?;
        let edges = compute_edges_between_nodes_simple(pool, &nodes, db_id).await?;

        Ok(CanvasLevel {
            parent_id: Some(tag_id.to_string()),
            parent_label: Some(tag_name.to_string()),
            breadcrumb: breadcrumb.to_vec(),
            nodes,
            edges,
        })
    }
}

async fn build_hint_level(
    pool: &sqlx::PgPool,
    parent_id: &str,
    hint_ids: &[String],
    db_id: &str,
) -> Result<CanvasLevel, AtomicCoreError> {
    if hint_ids.is_empty() {
        return Ok(CanvasLevel {
            parent_id: Some(parent_id.to_string()),
            parent_label: None,
            breadcrumb: vec![],
            nodes: vec![],
            edges: vec![],
        });
    }

    // Check if hints are tags or atoms
    let found_tags: Vec<(String, String)> =
        sqlx::query_as("SELECT id, name FROM tags WHERE id = ANY($1) AND db_id = $2")
            .bind(hint_ids)
            .bind(db_id)
            .fetch_all(pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let found_tag_map: HashMap<String, String> = found_tags.into_iter().collect();

    let breadcrumb = if parent_id.starts_with("cluster:") {
        let parts: Vec<&str> = parent_id.split(':').collect();
        if parts.len() >= 2 {
            let ancestor_id = parts[1];
            let mut bc = build_breadcrumb(pool, ancestor_id, db_id)
                .await
                .unwrap_or_default();
            bc.push(BreadcrumbEntry {
                id: parent_id.to_string(),
                label: "Cluster".to_string(),
            });
            bc
        } else {
            vec![]
        }
    } else {
        build_breadcrumb(pool, parent_id, db_id)
            .await
            .unwrap_or_default()
    };

    let parent_label = breadcrumb.last().map(|b| b.label.clone());

    if found_tag_map.len() == hint_ids.len() {
        // All are tags
        let tree = TagTree::load(pool, db_id).await?;

        let mut tag_nodes: Vec<(CanvasNode, i32)> = hint_ids
            .iter()
            .filter_map(|id| {
                let name = found_tag_map.get(id)?;
                let count = tree.transitive_count(id);
                let node_type = if tree.has_children(id) {
                    CanvasNodeType::Category
                } else {
                    CanvasNodeType::Tag
                };
                Some((
                    CanvasNode {
                        id: id.clone(),
                        node_type,
                        label: name.clone(),
                        atom_count: count,
                        children_ids: vec![],
                        dominant_tags: vec![],
                        centroid: None,
                    },
                    count,
                ))
            })
            .filter(|(_, count)| *count > 0)
            .collect();

        tag_nodes.sort_by(|a, b| b.1.cmp(&a.1));

        let nodes: Vec<CanvasNode> = if tag_nodes.len() <= MAX_TAGS_PER_LEVEL {
            tag_nodes.into_iter().map(|(n, _)| n).collect()
        } else {
            let (top, rest) = tag_nodes.split_at(TOP_TAGS_SHOWN);
            let mut result: Vec<CanvasNode> = top.iter().map(|(n, _)| n.clone()).collect();

            if rest.len() <= MAX_TAGS_FOR_CLUSTERING {
                let rest_ids: Vec<String> = rest.iter().map(|(n, _)| n.id.clone()).collect();
                let cluster_nodes =
                    cluster_tags_by_similarity(pool, &rest_ids, &tree, parent_id, db_id).await?;
                result.extend(cluster_nodes);
            } else {
                let group_nodes = group_tags_by_count(rest, &tree, parent_id);
                result.extend(group_nodes);
            }
            result
        };

        let edges = compute_edges_if_small(pool, &nodes, db_id).await?;

        Ok(CanvasLevel {
            parent_id: Some(parent_id.to_string()),
            parent_label,
            breadcrumb,
            nodes,
            edges,
        })
    } else {
        // Assume atoms
        let atom_ids = hint_ids.to_vec();
        if atom_ids.len() <= MAX_ATOMS_PER_LEVEL {
            let nodes = build_flat_atom_nodes(pool, &atom_ids, db_id).await?;
            let edges = compute_edges_for_atom_set(pool, &atom_ids, db_id).await?;

            Ok(CanvasLevel {
                parent_id: Some(parent_id.to_string()),
                parent_label,
                breadcrumb,
                nodes,
                edges,
            })
        } else {
            let nodes = cluster_atoms_into_groups(pool, &atom_ids, parent_id, db_id).await?;
            let edges = compute_edges_between_nodes_simple(pool, &nodes, db_id).await?;

            Ok(CanvasLevel {
                parent_id: Some(parent_id.to_string()),
                parent_label,
                breadcrumb,
                nodes,
                edges,
            })
        }
    }
}

// ==================== Trait Implementation ====================

#[async_trait]
impl ClusterStore for PostgresStorage {
    async fn compute_clusters(
        &self,
        min_similarity: f32,
        min_cluster_size: i32,
    ) -> StorageResult<Vec<AtomCluster>> {
        // Load semantic edges above threshold
        let edges: Vec<(String, String, f32)> = sqlx::query_as(
            "SELECT source_atom_id, target_atom_id, similarity_score
             FROM semantic_edges
             WHERE similarity_score >= $1 AND db_id = $2",
        )
        .bind(min_similarity)
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        if edges.is_empty() {
            return Ok(vec![]);
        }

        let labels = clustering::label_propagation(&edges);
        let groups = clustering::group_labels_into_clusters(&labels, min_cluster_size as usize);

        let mut clusters: Vec<AtomCluster> = Vec::new();
        for (i, atom_ids) in groups.into_iter().enumerate() {
            let dominant_tags = get_dominant_tags_for_cluster(&self.pool, &atom_ids, &self.db_id)
                .await
                .unwrap_or_default();
            clusters.push(AtomCluster {
                cluster_id: i as i32,
                atom_ids,
                dominant_tags,
            });
        }

        Ok(clusters)
    }

    async fn save_clusters(&self, clusters: &[AtomCluster]) -> StorageResult<()> {
        // Clear existing assignments
        sqlx::query("DELETE FROM atom_clusters WHERE db_id = $1")
            .bind(&self.db_id)
            .execute(&self.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        // Insert new assignments
        for cluster in clusters {
            for atom_id in &cluster.atom_ids {
                sqlx::query(
                    "INSERT INTO atom_clusters (atom_id, cluster_id, db_id) VALUES ($1, $2, $3)",
                )
                .bind(atom_id)
                .bind(cluster.cluster_id)
                .bind(&self.db_id)
                .execute(&self.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            }
        }

        Ok(())
    }

    async fn get_clusters(&self) -> StorageResult<Vec<AtomCluster>> {
        let count: Option<i64> = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT COUNT(*) FROM atom_clusters WHERE db_id = $1",
        )
        .bind(&self.db_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
        let count = count.unwrap_or(0);

        if count == 0 {
            let clusters = self.compute_clusters(0.5, 2).await?;
            self.save_clusters(&clusters).await?;
            return Ok(clusters);
        }

        // Rebuild from cached assignments using STRING_AGG (Postgres equivalent of GROUP_CONCAT)
        let rows: Vec<(i32, String)> = sqlx::query_as(
            "SELECT ac.cluster_id, STRING_AGG(ac.atom_id, ',')
             FROM atom_clusters ac
             WHERE ac.db_id = $1
             GROUP BY ac.cluster_id
             ORDER BY COUNT(*) DESC",
        )
        .bind(&self.db_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

        let mut clusters: Vec<AtomCluster> = Vec::new();
        for (cluster_id, atom_ids_str) in rows {
            let atom_ids: Vec<String> = atom_ids_str.split(',').map(|s| s.to_string()).collect();
            let dominant_tags = get_dominant_tags_for_cluster(&self.pool, &atom_ids, &self.db_id)
                .await
                .unwrap_or_default();
            clusters.push(AtomCluster {
                cluster_id,
                atom_ids,
                dominant_tags,
            });
        }

        Ok(clusters)
    }

    async fn get_canvas_level(
        &self,
        parent_id: Option<&str>,
        children_hint: Option<Vec<String>>,
    ) -> StorageResult<CanvasLevel> {
        match (parent_id, &children_hint) {
            (None, _) => build_root_level(&self.pool, &self.db_id).await,
            (Some(pid), Some(hint)) => build_hint_level(&self.pool, pid, hint, &self.db_id).await,
            (Some(pid), None) => build_tag_level(&self.pool, pid, &self.db_id).await,
        }
    }

    async fn enrich_clusters_with_tags(
        &self,
        mut clusters: Vec<AtomCluster>,
    ) -> StorageResult<Vec<AtomCluster>> {
        // Overrides the trait's no-op default. Without this, clusters
        // computed in-process (the global-canvas path) reach the label
        // builder with empty dominant_tags and every canvas pill renders
        // as "Cluster N" — SQLite has enriched here since day one; the
        // Postgres side silently inherited the default. Uses the same
        // child-tags-only query the canvas level builder labels with, so
        // category roots ("Topics") never headline a cluster.
        for cluster in &mut clusters {
            cluster.dominant_tags =
                get_dominant_tags_for_atoms(&self.pool, &cluster.atom_ids, &self.db_id).await?;
        }
        Ok(clusters)
    }
}
