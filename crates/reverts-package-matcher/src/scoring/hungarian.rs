//! In-tree Kuhn-Munkres / Hungarian algorithm for maximum-weight bipartite assignment.
//!
//! Implements the classic O(n^3) successive-shortest-path Hungarian algorithm
//! (Jonker-Volgenant style) for the minimum-cost assignment, then converts the
//! input weight matrix to the negated cost matrix so that the resulting
//! assignment maximises `Σ cost[i][assign[i]]`.
//!
//! Non-square inputs are padded with synthetic zero-weight entries; if a row
//! has no real column assigned (which can happen when there are fewer columns
//! than rows), it is reported as `usize::MAX` so callers can filter it out.

const INF: f64 = f64::INFINITY;

/// Returns the column index assigned to each row, maximizing
/// `Σ cost[i][assign[i]]` over all assignments.
///
/// If `cost` is non-square (m rows × n cols), the result has `m` entries.
/// If a row cannot be assigned (e.g., `n < m`), its slot is `usize::MAX`
/// (callers can filter such rows).
///
/// For empty input, returns an empty vector.
#[must_use]
pub fn assign_max_weight(cost: &[Vec<f64>]) -> Vec<usize> {
    let m = cost.len();
    if m == 0 {
        return Vec::new();
    }
    let n_in = cost[0].len();
    if n_in == 0 {
        return vec![usize::MAX; m];
    }
    let n = m.max(n_in);

    // Build a negated, padded cost matrix so that the minimum-cost
    // formulation of the algorithm yields the maximum-weight assignment.
    // Padded (synthetic) entries have weight 0 → cost 0.
    let mut c = vec![vec![0.0_f64; n]; n];
    for (i, row) in cost.iter().enumerate().take(m) {
        for (j, &weight) in row.iter().enumerate().take(n_in) {
            c[i][j] = -weight;
        }
    }

    // 1-indexed potentials and bookkeeping arrays per the classic algorithm.
    let mut u = vec![0.0_f64; n + 1];
    let mut v = vec![0.0_f64; n + 1];
    let mut p = vec![0_usize; n + 1];
    let mut way = vec![0_usize; n + 1];

    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0_usize;
        let mut minv = vec![INF; n + 1];
        let mut used = vec![false; n + 1];

        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = INF;
            let mut j1 = 0_usize;

            for j in 1..=n {
                if !used[j] {
                    let cur = c[i0 - 1][j - 1] - u[i0] - v[j];
                    if cur < minv[j] {
                        minv[j] = cur;
                        way[j] = j0;
                    }
                    if minv[j] < delta {
                        delta = minv[j];
                        j1 = j;
                    }
                }
            }
            for j in 0..=n {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        while j0 != 0 {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
        }
    }

    // p[j] = row (1-indexed) matched to column j. Build assign[row] = col.
    let mut assign = vec![usize::MAX; m];
    for (j, &i_row) in p.iter().enumerate().take(n + 1).skip(1) {
        if i_row == 0 {
            continue;
        }
        let row = i_row - 1;
        let col = j - 1;
        // Only record assignments to real columns; synthetic padding columns
        // (col >= n_in) are reported as `usize::MAX` so the caller can drop
        // them. Real rows assigned to a synthetic column count as unassigned.
        if row < m && col < n_in {
            assign[row] = col;
        }
    }
    assign
}

#[cfg(test)]
mod tests {
    use super::assign_max_weight;

    #[test]
    fn hungarian_two_by_two_picks_diagonal_when_better() {
        let cost = vec![vec![5.0, 1.0], vec![1.0, 5.0]];
        let assign = assign_max_weight(&cost);
        assert_eq!(assign, vec![0, 1]);
    }

    #[test]
    fn hungarian_two_by_two_picks_anti_diagonal_when_better() {
        let cost = vec![vec![1.0, 5.0], vec![5.0, 1.0]];
        let assign = assign_max_weight(&cost);
        assert_eq!(assign, vec![1, 0]);
    }

    #[test]
    fn hungarian_three_by_three_picks_globally_optimal() {
        // Greedy would pick (0,0)=10, leaving (1,1)=8 + (2,2)=1, total = 19
        // Optimal: (0,2)=9, (1,1)=8, (2,0)=7, total = 24
        let cost = vec![
            vec![10.0, 5.0, 9.0],
            vec![4.0, 8.0, 3.0],
            vec![7.0, 2.0, 1.0],
        ];
        let assign = assign_max_weight(&cost);
        assert_eq!(assign, vec![2, 1, 0]);
    }

    #[test]
    fn hungarian_handles_zero_weight_rows() {
        let cost = vec![vec![0.0, 0.0], vec![1.0, 2.0]];
        let assign = assign_max_weight(&cost);
        // Row 1 takes column 1 (higher weight); row 0 takes whatever's left.
        assert_eq!(assign[1], 1);
    }

    #[test]
    fn hungarian_empty_returns_empty() {
        let cost: Vec<Vec<f64>> = Vec::new();
        let assign = assign_max_weight(&cost);
        assert!(assign.is_empty());
    }

    #[test]
    fn hungarian_non_square_pads_with_zero() {
        // 2 rows, 3 columns — algorithm should still produce a valid assignment
        // for the 2 rows (each gets a column; the 3rd column unused).
        let cost = vec![vec![10.0, 1.0, 1.0], vec![1.0, 10.0, 1.0]];
        let assign = assign_max_weight(&cost);
        assert_eq!(assign.len(), 2);
        assert_eq!(assign[0], 0);
        assert_eq!(assign[1], 1);
    }
}
