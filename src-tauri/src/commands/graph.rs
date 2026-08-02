//! Graph and statistics projections of the catalog: tag co-occurrence, the
//! photo↔tag bipartite graph, the combined library graph, and catalog stats.
//!
//! These return `serde_json::Value` shaped for the frontend's d3 views.

use super::*;
use tauri::State;

/// A node in a tag/photo graph (Tag Graph module).
#[derive(serde::Serialize)]
struct GraphNode {
    id: i64,
    label: String,
    count: i64,
}

/// Photo↔tag bipartite graph.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BipartiteGraph {
    photos: Vec<GraphNode>,
    tags: Vec<GraphNode>,
    /// `(photoId, tagId)` assignment edges.
    edges: Vec<(i64, i64)>,
}

/// Photo↔tag bipartite graph data.
#[tauri::command]
pub async fn photo_tag_graph(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    with_catalog_blocking(&state, move |c| {
        let (photos, tags, edges) = c.photo_tag_graph()?;
        let g = BipartiteGraph {
            photos: photos
                .into_iter()
                .map(|(id, label, count)| GraphNode { id, label, count })
                .collect(),
            tags: tags
                .into_iter()
                .map(|(id, label, count)| GraphNode { id, label, count })
                .collect(),
            edges,
        };
        Ok(serde_json::to_value(g).unwrap_or_default())
    })
    .await
}

/// Library graph: tags, cameras, and their relationships.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryGraph {
    /// Tag nodes (id = tag id).
    tags: Vec<GraphNode>,
    /// Camera nodes (id = index into this vec).
    cameras: Vec<GraphNode>,
    /// `(tagA, tagB, sharedPhotos)`.
    cooc_edges: Vec<(i64, i64, i64)>,
    /// `(parentTagId, childTagId)`.
    hierarchy_edges: Vec<(i64, i64)>,
    /// `(cameraIdx, tagId, sharedPhotos)`.
    camera_edges: Vec<(i64, i64, i64)>,
}

/// Library graph data: tags, cameras, tag/camera relationships.
#[tauri::command]
pub async fn library_graph(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    with_catalog_blocking(&state, move |c| {
        let (tags, cameras, cooc_edges, hierarchy_edges, camera_edges) = c.library_graph()?;

        // Build camera nodes with index as id
        let camera_nodes: Vec<GraphNode> = cameras
            .iter()
            .enumerate()
            .map(|(idx, (model, count))| GraphNode {
                id: idx as i64,
                label: model.clone(),
                count: *count,
            })
            .collect();

        // Build camera model to index map for remapping camera_edges
        let camera_model_to_idx: std::collections::HashMap<_, _> = cameras
            .into_iter()
            .enumerate()
            .map(|(idx, (model, _))| (model, idx as i64))
            .collect();

        // Remap camera_edges from camera_model → tag_id to camera_idx → tag_id
        let remapped_camera_edges: Vec<(i64, i64, i64)> = camera_edges
            .into_iter()
            .filter_map(|(model, tag_id, count)| {
                camera_model_to_idx.get(&model).map(|&idx| (idx, tag_id, count))
            })
            .collect();

        let g = LibraryGraph {
            tags: tags
                .into_iter()
                .map(|(id, label, count)| GraphNode { id, label, count })
                .collect(),
            cameras: camera_nodes,
            cooc_edges,
            hierarchy_edges,
            camera_edges: remapped_camera_edges,
        };
        Ok(serde_json::to_value(g).unwrap_or_default())
    })
    .await
}

/// Catalog-wide statistics (Statistics module).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogStats {
    total_photos: i64,
    with_capture_time: i64,
    first_month: Option<String>,
    last_month: Option<String>,
    timeline: Vec<(String, i64)>,
    hours: Vec<i64>,
    weekdays: Vec<i64>,
    top_tags: Vec<GraphNode>,
    cameras: Vec<(String, i64)>,
    lenses: Vec<(String, i64)>,
    focal_lengths: Vec<(f64, i64)>,
    ratings: Vec<i64>,
    /// The 3 busiest single days (`YYYY-MM-DD`, count).
    top_days: Vec<(String, i64)>,
    /// Photos with an implausible capture date (pre-1950), excluded from time stats.
    invalid_dates: i64,
}

/// Catalog-wide statistics: timeline, hours, weekdays, top tags, cameras, lenses,
/// focal lengths, and rating distribution.
///
/// When `tag_id`, `album_id`, or `batch_id` are set the stats are scoped to
/// that subset (AND-combined). `tag_id` includes descendant tags. All `None` →
/// whole-catalog stats (original behaviour).
#[tauri::command]
pub async fn catalog_stats(
    state: State<'_, AppState>,
    tag_id: Option<i64>,
    album_id: Option<i64>,
    batch_id: Option<i64>,
) -> Result<serde_json::Value, String> {
    with_catalog_blocking(&state, move |c| {
        let raw = c.catalog_stats(tag_id, album_id, batch_id)?;
        let stats = CatalogStats {
            total_photos: raw.total_photos,
            with_capture_time: raw.with_capture_time,
            first_month: raw.first_month,
            last_month: raw.last_month,
            timeline: raw.timeline,
            hours: raw.hours,
            weekdays: raw.weekdays,
            top_tags: raw
                .top_tags
                .into_iter()
                .map(|(id, label, count)| GraphNode { id, label, count })
                .collect(),
            cameras: raw.cameras,
            lenses: raw.lenses,
            focal_lengths: raw.focal_lengths,
            ratings: raw.ratings,
            top_days: raw.top_days,
            invalid_dates: raw.invalid_dates,
        };
        Ok(serde_json::to_value(stats).unwrap_or_default())
    })
    .await
}

