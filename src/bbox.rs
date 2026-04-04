//! Bounding-box computation methods for the per-block motion map.
//!
//! The motion map is a flat `&[f32]` grid of `cols × rows` blocks, each
//! holding a score (e.g. mean MAD). [`compute_bbox`] thresholds the map and
//! returns the smallest axis-aligned rectangle that covers every "active"
//! block, expressed in pixel coordinates relative to the full frame.
//!
//! Four methods are provided via [`BboxMethod`]:
//! - [`BboxMethod::Union`] — tight min/max envelope (default).
//! - [`BboxMethod::Percentile`] — trim outlier blocks from each edge.
//! - [`BboxMethod::DensityFilter`] — require a minimum number of active
//!   blocks per row/column.
//! - [`BboxMethod::Erosion`] — require a minimum number of active
//!   4-connected neighbours.

use crate::BLOCK;

// ── Public API ───────────────────────────────────────────────────────────────

/// An axis-aligned bounding box in pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bbox {
    /// Left edge in pixels.
    pub x: u32,
    /// Top edge in pixels.
    pub y: u32,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
}

impl Bbox {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }
}

/// Strategy used when computing the active-region bounding box.
#[derive(Copy, Clone, PartialEq, Debug, Default)]
pub enum BboxMethod {
    /// Tight axis-aligned envelope of every block whose score ≥ threshold.
    /// One "hot" outlier anywhere in the frame will expand the box to include
    /// it.  This is the fastest method and the default.
    #[default]
    Union,

    /// Trim `p` percent of active-block coordinates from each edge before
    /// computing the envelope.  `p` is clamped to `[0, 49.9]`.
    ///
    /// A value of `5.0` means the 5 % most extreme active blocks on each side
    /// are ignored, making the result robust to isolated hot pixels.
    Percentile(f32),

    /// Only include a row (or column) if it contains at least `n` active
    /// blocks.  Strips sparse border rows/columns while preserving dense
    /// regions.
    DensityFilter(usize),

    /// A block is only considered active if it **and** at least `n` of its
    /// 4-connected neighbours are also active.  Removes isolated hot spots
    /// while preserving large contiguous regions.
    Erosion(usize),
}

/// Compute the active-region bounding box from a flat block-score map.
///
/// * `map`       — per-block scores, row-major, length `cols × rows`.
/// * `cols/rows` — grid dimensions.
/// * `fw/fh`     — full frame width/height in pixels (used for pixel output).
/// * `threshold` — minimum score for a block to be considered active.
/// * `method`    — how to handle outlier blocks (see [`BboxMethod`]).
///
/// Returns `None` when no block survives the method's filter.
pub fn compute_bbox(
    map: &[f32],
    cols: usize,
    rows: usize,
    fw: u32,
    fh: u32,
    threshold: f32,
    method: BboxMethod,
) -> Option<Bbox> {
    let (min_col, min_row, max_col, max_row) = match method {
        BboxMethod::Union => bbox_union(map, cols, rows, threshold)?,
        BboxMethod::Percentile(p) => bbox_percentile(map, cols, rows, threshold, p)?,
        BboxMethod::DensityFilter(n) => bbox_density_filter(map, cols, rows, threshold, n)?,
        BboxMethod::Erosion(n) => bbox_erosion(map, cols, rows, threshold, n)?,
    };
    Some(blocks_to_pixels(min_col, min_row, max_col, max_row, fw, fh))
}

// ── Private helpers ──────────────────────────────────────────────────────────

/// Convert block-grid coordinates to pixel coordinates.
fn blocks_to_pixels(
    min_col: usize,
    min_row: usize,
    max_col: usize,
    max_row: usize,
    fw: u32,
    fh: u32,
) -> Bbox {
    let w = fw as usize;
    let h = fh as usize;
    let x = (min_col * BLOCK) as u32;
    let y = (min_row * BLOCK) as u32;
    let bw = ((max_col + 1) * BLOCK).min(w) as u32 - x;
    let bh = ((max_row + 1) * BLOCK).min(h) as u32 - y;
    Bbox::new(x, y, bw, bh)
}

/// Tight min/max envelope — includes every active block.
fn bbox_union(
    map: &[f32],
    cols: usize,
    rows: usize,
    threshold: f32,
) -> Option<(usize, usize, usize, usize)> {
    let mut min_col = cols;
    let mut max_col = 0usize;
    let mut min_row = rows;
    let mut max_row = 0usize;
    let mut found = false;

    for by in 0..rows {
        for bx in 0..cols {
            if map[by * cols + bx] >= threshold {
                min_col = min_col.min(bx);
                max_col = max_col.max(bx);
                min_row = min_row.min(by);
                max_row = max_row.max(by);
                found = true;
            }
        }
    }

    found.then_some((min_col, min_row, max_col, max_row))
}

/// Percentile-trimmed envelope.
fn bbox_percentile(
    map: &[f32],
    cols: usize,
    rows: usize,
    threshold: f32,
    p: f32,
) -> Option<(usize, usize, usize, usize)> {
    let p = p.clamp(0.0, 49.9);

    let mut xs: Vec<usize> = Vec::new();
    let mut ys: Vec<usize> = Vec::new();
    for by in 0..rows {
        for bx in 0..cols {
            if map[by * cols + bx] >= threshold {
                xs.push(bx);
                ys.push(by);
            }
        }
    }

    if xs.is_empty() {
        return None;
    }

    xs.sort_unstable();
    ys.sort_unstable();

    let n = xs.len();
    let trim = ((p / 100.0 * n as f32).floor() as usize).min((n - 1) / 2);

    Some((xs[trim], ys[trim], xs[n - 1 - trim], ys[n - 1 - trim]))
}

/// Density-filtered envelope — only rows/columns with ≥ `min_n` active blocks.
fn bbox_density_filter(
    map: &[f32],
    cols: usize,
    rows: usize,
    threshold: f32,
    min_n: usize,
) -> Option<(usize, usize, usize, usize)> {
    // Count active blocks per row and per column.
    let row_counts: Vec<usize> = (0..rows)
        .map(|by| {
            (0..cols)
                .filter(|&bx| map[by * cols + bx] >= threshold)
                .count()
        })
        .collect();
    let col_counts: Vec<usize> = (0..cols)
        .map(|bx| {
            (0..rows)
                .filter(|&by| map[by * cols + bx] >= threshold)
                .count()
        })
        .collect();

    let qual_rows: Vec<usize> = (0..rows).filter(|&by| row_counts[by] >= min_n).collect();
    let qual_cols: Vec<usize> = (0..cols).filter(|&bx| col_counts[bx] >= min_n).collect();

    if qual_rows.is_empty() || qual_cols.is_empty() {
        return None;
    }

    Some((
        *qual_cols.iter().min().unwrap(),
        *qual_rows.iter().min().unwrap(),
        *qual_cols.iter().max().unwrap(),
        *qual_rows.iter().max().unwrap(),
    ))
}

/// Morphological-erosion envelope — blocks must have ≥ `min_neighbors` active
/// 4-connected neighbours to survive.
fn bbox_erosion(
    map: &[f32],
    cols: usize,
    rows: usize,
    threshold: f32,
    min_neighbors: usize,
) -> Option<(usize, usize, usize, usize)> {
    // Build active mask.
    let active: Vec<bool> = map.iter().map(|&v| v >= threshold).collect();

    // Erode: keep only blocks with enough active 4-neighbours.
    let neighbor_count = |bx: usize, by: usize| -> usize {
        let mut n = 0usize;
        if bx > 0 && active[by * cols + bx - 1] {
            n += 1;
        }
        if bx + 1 < cols && active[by * cols + bx + 1] {
            n += 1;
        }
        if by > 0 && active[(by - 1) * cols + bx] {
            n += 1;
        }
        if by + 1 < rows && active[(by + 1) * cols + bx] {
            n += 1;
        }
        n
    };

    let mut min_col = cols;
    let mut max_col = 0usize;
    let mut min_row = rows;
    let mut max_row = 0usize;
    let mut found = false;

    for by in 0..rows {
        for bx in 0..cols {
            if active[by * cols + bx] && neighbor_count(bx, by) >= min_neighbors {
                min_col = min_col.min(bx);
                max_col = max_col.max(bx);
                min_row = min_row.min(by);
                max_row = max_row.max(by);
                found = true;
            }
        }
    }

    found.then_some((min_col, min_row, max_col, max_row))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const THRESH: f32 = 1.0;

    /// Build a `cols × rows` map with 0.0 everywhere except the listed blocks
    /// which are set to 2.0 (above THRESH).
    fn make_map(cols: usize, rows: usize, active: &[(usize, usize)]) -> Vec<f32> {
        let mut m = vec![0.0f32; cols * rows];
        for &(bx, by) in active {
            m[by * cols + bx] = 2.0;
        }
        m
    }

    // Helper: run compute_bbox on a 4×4 grid (64×64 px frame).
    fn bbox4(active: &[(usize, usize)], method: BboxMethod) -> Option<Bbox> {
        let map = make_map(4, 4, active);
        compute_bbox(&map, 4, 4, 64, 64, THRESH, method)
    }

    fn b(x: u32, y: u32, w: u32, h: u32) -> Option<Bbox> {
        Some(Bbox::new(x, y, w, h))
    }

    // ── All-zero maps ────────────────────────────────────────────────────────

    #[test]
    fn test_all_zero_union() {
        assert_eq!(bbox4(&[], BboxMethod::Union), None);
    }

    #[test]
    fn test_all_zero_percentile() {
        assert_eq!(bbox4(&[], BboxMethod::Percentile(10.0)), None);
    }

    #[test]
    fn test_all_zero_density_filter() {
        assert_eq!(bbox4(&[], BboxMethod::DensityFilter(1)), None);
    }

    #[test]
    fn test_all_zero_erosion() {
        assert_eq!(bbox4(&[], BboxMethod::Erosion(1)), None);
    }

    // ── Single active block ──────────────────────────────────────────────────

    // Block (1,2) in a 4×4/64×64 grid → pixel rect Bbox{x:16, y:32, w:16, h:16}.
    #[test]
    fn test_single_block_union() {
        assert_eq!(bbox4(&[(1, 2)], BboxMethod::Union), b(16, 32, 16, 16));
    }

    #[test]
    fn test_single_block_percentile_zero() {
        assert_eq!(bbox4(&[(1, 2)], BboxMethod::Percentile(0.0)), b(16, 32, 16, 16));
    }

    /// With only 1 active block the trim is clamped to 0 — result unchanged.
    #[test]
    fn test_single_block_percentile_fifty() {
        assert_eq!(bbox4(&[(1, 2)], BboxMethod::Percentile(50.0)), b(16, 32, 16, 16));
    }

    #[test]
    fn test_single_block_density_filter_1() {
        // Row 2 has 1 active block ≥ 1; col 1 has 1 active block ≥ 1 → survives.
        assert_eq!(bbox4(&[(1, 2)], BboxMethod::DensityFilter(1)), b(16, 32, 16, 16));
    }

    #[test]
    fn test_single_block_density_filter_2() {
        // Row 2 has 1 < 2; col 1 has 1 < 2 → neither qualifies.
        assert_eq!(bbox4(&[(1, 2)], BboxMethod::DensityFilter(2)), None);
    }

    #[test]
    fn test_single_block_erosion_1() {
        // Isolated block has 0 active neighbours < 1 → removed.
        assert_eq!(bbox4(&[(1, 2)], BboxMethod::Erosion(1)), None);
    }

    #[test]
    fn test_single_block_erosion_0() {
        // Requiring 0 neighbours: block survives regardless.
        assert_eq!(bbox4(&[(1, 2)], BboxMethod::Erosion(0)), b(16, 32, 16, 16));
    }

    // ── Outlier rejection ────────────────────────────────────────────────────

    // Map: outlier at (0,0) + 2×2 interior cluster at (1,1),(1,2),(2,1),(2,2).
    fn outlier_active() -> Vec<(usize, usize)> {
        vec![(0, 0), (1, 1), (1, 2), (2, 1), (2, 2)]
    }

    #[test]
    fn test_outlier_union_includes_outlier() {
        // Union stretches to include (0,0).
        assert_eq!(bbox4(&outlier_active(), BboxMethod::Union), b(0, 0, 48, 48));
    }

    #[test]
    fn test_outlier_percentile_excludes_outlier() {
        // Active cols sorted: [0,1,1,2,2] (5 entries), trim = floor(0.25*5)=1.
        // min_col = sorted[1] = 1, max_col = sorted[3] = 2. Same for rows.
        assert_eq!(
            bbox4(&outlier_active(), BboxMethod::Percentile(25.0)),
            b(16, 16, 32, 32)
        );
    }

    #[test]
    fn test_outlier_erosion_excludes_outlier() {
        // (0,0) has 0 active neighbours → removed. Interior 2×2 blocks each
        // have 2 active neighbours ≥ 1 → survive.
        assert_eq!(bbox4(&outlier_active(), BboxMethod::Erosion(1)), b(16, 16, 32, 32));
    }

    // ── Fully active map ─────────────────────────────────────────────────────

    fn all_active_4x4() -> Vec<(usize, usize)> {
        (0..4)
            .flat_map(|by| (0..4).map(move |bx| (bx, by)))
            .collect()
    }

    #[test]
    fn test_fully_active_union() {
        assert_eq!(bbox4(&all_active_4x4(), BboxMethod::Union), b(0, 0, 64, 64));
    }

    #[test]
    fn test_fully_active_density_filter() {
        assert_eq!(bbox4(&all_active_4x4(), BboxMethod::DensityFilter(1)), b(0, 0, 64, 64));
    }

    #[test]
    fn test_fully_active_erosion_1() {
        // Every block in a 4×4 grid has ≥ 2 active neighbours → all survive.
        assert_eq!(bbox4(&all_active_4x4(), BboxMethod::Erosion(1)), b(0, 0, 64, 64));
    }

    // ── DensityFilter: top-row strip ─────────────────────────────────────────

    #[test]
    fn test_density_filter_top_row_n1() {
        // Active: (0,0),(1,0),(2,0) — row 0 has 3 blocks; each col has 1 block.
        // DensityFilter(1): row 0 qualifies; cols 0,1,2 qualify.
        assert_eq!(
            bbox4(&[(0, 0), (1, 0), (2, 0)], BboxMethod::DensityFilter(1)),
            b(0, 0, 48, 16)
        );
    }

    #[test]
    fn test_density_filter_top_row_n2() {
        // DensityFilter(2): row 0 qualifies (3 ≥ 2) but each col only has 1 < 2
        // active block → no column qualifies → None.
        assert_eq!(
            bbox4(&[(0, 0), (1, 0), (2, 0)], BboxMethod::DensityFilter(2)),
            None
        );
    }

    // ── Erosion: 3×3 cluster in 5×5 grid ────────────────────────────────────

    fn bbox5(active: &[(usize, usize)], method: BboxMethod) -> Option<Bbox> {
        let map = make_map(5, 5, active);
        compute_bbox(&map, 5, 5, 80, 80, THRESH, method)
    }

    fn cluster_3x3() -> Vec<(usize, usize)> {
        (1..=3)
            .flat_map(|by| (1..=3).map(move |bx| (bx, by)))
            .collect()
    }

    #[test]
    fn test_erosion_3x3_cluster_n1() {
        // All 9 blocks have ≥ 1 active neighbour → all survive.
        assert_eq!(bbox5(&cluster_3x3(), BboxMethod::Erosion(1)), b(16, 16, 48, 48));
    }

    #[test]
    fn test_erosion_3x3_cluster_n4() {
        // Only the center block (2,2) has 4 active neighbours; all edge/corner
        // blocks of the 3×3 cluster have ≤ 3 active neighbours → only (2,2) survives.
        assert_eq!(bbox5(&cluster_3x3(), BboxMethod::Erosion(4)), b(32, 32, 16, 16));
    }

    // ── Default ──────────────────────────────────────────────────────────────

    #[test]
    fn test_default_is_union() {
        assert_eq!(BboxMethod::default(), BboxMethod::Union);
    }

    // ── Pixel clamping: frame not a multiple of BLOCK ────────────────────────

    #[test]
    fn test_partial_frame_clamping() {
        // fw=20, fh=20 → 2×2 block grid. Block (1,1) overhangs the frame
        // boundary: pixel extent would be [16, 48) × [16, 48) but the frame
        // is only 20 px wide/tall, so w and h are clamped to 4.
        let map = make_map(2, 2, &[(1, 1)]);
        assert_eq!(
            compute_bbox(&map, 2, 2, 20, 20, THRESH, BboxMethod::Union),
            b(16, 16, 4, 4)
        );
    }

    // ── DensityFilter extras ─────────────────────────────────────────────────

    #[test]
    fn test_density_filter_outlier_n2() {
        // outlier at (0,0) + 2×2 interior cluster.
        // Row 0 has 1 block < 2 → excluded.  Col 0 has 1 block < 2 → excluded.
        // Rows 1–2 and cols 1–2 each have 2 blocks ≥ 2 → include only the cluster.
        assert_eq!(
            bbox4(&outlier_active(), BboxMethod::DensityFilter(2)),
            b(16, 16, 32, 32)
        );
    }

    #[test]
    fn test_density_filter_zero_includes_all_rows_cols() {
        // min_n=0: every row and column qualifies (count >= 0 is always true),
        // so the result spans the entire grid regardless of where the active
        // blocks actually are.
        assert_eq!(bbox4(&[(1, 2)], BboxMethod::DensityFilter(0)), b(0, 0, 64, 64));
    }

    // ── Percentile extras ────────────────────────────────────────────────────

    #[test]
    fn test_percentile_p0_equals_union() {
        // p=0 means trim=0 → identical to the min/max scan of Union.
        assert_eq!(
            bbox4(&outlier_active(), BboxMethod::Percentile(0.0)),
            bbox4(&outlier_active(), BboxMethod::Union)
        );
    }

    #[test]
    fn test_percentile_two_active_blocks_clamped() {
        // xs=[0,3], ys=[0,3], n=2.  Even at p=49 the trim is
        // floor(0.49 × 2)=0 — the clamp ensures at least both blocks remain.
        assert_eq!(bbox4(&[(0, 0), (3, 3)], BboxMethod::Percentile(49.0)), b(0, 0, 64, 64));
    }

    #[test]
    fn test_percentile_diagonal_trims_to_center() {
        // Active diagonal: (0,0),(1,1),(2,2).
        // xs=[0,1,2], ys=[0,1,2], n=3.
        // p=34 → trim = floor(0.34×3)=floor(1.02)=1.
        // min/max after trim: xs[1]=1, xs[1]=1 → only block (1,1) remains.
        assert_eq!(
            bbox4(&[(0, 0), (1, 1), (2, 2)], BboxMethod::Percentile(34.0)),
            b(16, 16, 16, 16)
        );
    }

    #[test]
    fn test_percentile_asymmetric_outlier() {
        // Outlier at (0,1) + dense cluster (2,1),(3,1),(2,2),(3,2).
        // xs=[0,2,2,3,3], n=5.  p=20 → trim=floor(0.2×5)=1.
        // min_col=xs[1]=2, max_col=xs[3]=3.  Same logic trims min_row to 1.
        // Outlier column 0 is excluded; cluster [2,3]×[1,2] remains.
        assert_eq!(
            bbox4(
                &[(0, 1), (2, 1), (3, 1), (2, 2), (3, 2)],
                BboxMethod::Percentile(20.0)
            ),
            b(32, 16, 32, 32)
        );
    }

    // ── Erosion: horizontal line ─────────────────────────────────────────────

    #[test]
    fn test_erosion_horizontal_line_n1() {
        // Full row 2: (0,2),(1,2),(2,2),(3,2).
        // Every block has ≥ 1 active neighbour (end blocks have 1; inner have 2).
        // All 4 survive → row 2, cols 0–3.
        assert_eq!(
            bbox4(&[(0, 2), (1, 2), (2, 2), (3, 2)], BboxMethod::Erosion(1)),
            b(0, 32, 64, 16)
        );
    }

    #[test]
    fn test_erosion_horizontal_line_n2() {
        // Same row. End blocks (0,2) and (3,2) have only 1 neighbour < 2 →
        // removed.  Inner blocks (1,2) and (2,2) each have 2 neighbours → survive.
        assert_eq!(
            bbox4(&[(0, 2), (1, 2), (2, 2), (3, 2)], BboxMethod::Erosion(2)),
            b(16, 32, 32, 16)
        );
    }

    // ── Erosion: 2×2 cluster ─────────────────────────────────────────────────

    #[test]
    fn test_erosion_2x2_cluster_n2() {
        // Each block in a 2×2 cluster has exactly 2 active 4-neighbours.
        // n=2: all survive.
        assert_eq!(
            bbox4(&[(1, 1), (1, 2), (2, 1), (2, 2)], BboxMethod::Erosion(2)),
            b(16, 16, 32, 32)
        );
    }

    #[test]
    fn test_erosion_2x2_cluster_n3() {
        // n=3: maximum 4-neighbours in a 2×2 cluster is 2 < 3 → all removed.
        assert_eq!(
            bbox4(&[(1, 1), (1, 2), (2, 1), (2, 2)], BboxMethod::Erosion(3)),
            None
        );
    }
}
