use std::cmp;

pub struct Dtw<T: Copy> {
    state_m: usize,
    state_n: usize,
    distance_fn: fn(T, T) -> f32,
    state_similarity: Option<f32>,
    distance_cost_matrix: Option<Vec<Vec<f32>>>,
    work_prev: Vec<f32>,
    work_curr: Vec<f32>,
}
impl<T: Copy> Dtw<T> {
    pub fn compute_optimal_path(&mut self, first_sequence: &[T], second_sequence: &[T]) -> f32 {
        self.state_m = first_sequence.len();
        self.state_n = second_sequence.len();
        let mut distance_cost_matrix: Vec<Vec<f32>> = infinity_matrix(self.state_m, self.state_n);
        distance_cost_matrix[0][0] = (self.distance_fn)(first_sequence[0], second_sequence[0]);
        for (row_index, first_sequence_item) in
            first_sequence.iter().enumerate().take(self.state_m).skip(1)
        {
            let cost = (self.distance_fn)(*first_sequence_item, second_sequence[0]);
            distance_cost_matrix[row_index][0] = cost + distance_cost_matrix[row_index - 1][0];
        }
        for (column_index, second_sequence_item) in second_sequence
            .iter()
            .enumerate()
            .take(self.state_n)
            .skip(1)
        {
            let cost = (self.distance_fn)(first_sequence[0], *second_sequence_item);
            distance_cost_matrix[0][column_index] =
                cost + distance_cost_matrix[0][column_index - 1];
        }
        for (row_index, first_sequence_item) in
            first_sequence.iter().enumerate().take(self.state_m).skip(1)
        {
            for (column_index, second_sequence_item) in second_sequence
                .iter()
                .enumerate()
                .take(self.state_n)
                .skip(1)
            {
                let cost = (self.distance_fn)(*first_sequence_item, *second_sequence_item);
                let insertion = distance_cost_matrix[row_index - 1][column_index];
                let deletion = distance_cost_matrix[row_index][column_index - 1];
                let matches = distance_cost_matrix[row_index - 1][column_index - 1];
                let min_value = [insertion, deletion, matches]
                    .iter()
                    .fold(f32::INFINITY, |a, &b| a.min(b));
                distance_cost_matrix[row_index][column_index] = cost + min_value;
            }
        }
        let similarity = distance_cost_matrix[self.state_m - 1][self.state_n - 1];
        self.distance_cost_matrix = Option::Some(distance_cost_matrix);
        self.state_similarity = Option::Some(similarity);
        similarity
    }
    pub fn compute_optimal_path_with_window(
        &mut self,
        first_sequence: &[T],
        second_sequence: &[T],
        w: u16,
    ) -> f32 {
        self.state_m = first_sequence.len();
        self.state_n = second_sequence.len();
        let window = cmp::max(
            w as usize,
            abs_diff(first_sequence.len(), second_sequence.len()),
        );

        let mut distance_cost_matrix: Vec<Vec<f32>> =
            infinity_matrix(self.state_m + 1, self.state_n + 1);
        distance_cost_matrix[0][0] = 0.;
        for row_index in 1..=self.state_m {
            let start_index = if row_index > window {
                cmp::max(1, row_index - window)
            } else {
                1
            };
            for column_index in start_index..cmp::min(self.state_n + 1, row_index + window) {
                let cost = (self.distance_fn)(
                    first_sequence[row_index - 1],
                    second_sequence[column_index - 1],
                );
                let insertion = distance_cost_matrix[row_index - 1][column_index];
                let deletion = distance_cost_matrix[row_index][column_index - 1];
                let matches = distance_cost_matrix[row_index - 1][column_index - 1];
                let min_value = [insertion, deletion, matches]
                    .iter()
                    .fold(f32::INFINITY, |a, &b| a.min(b));
                distance_cost_matrix[row_index][column_index] = cost + min_value;
            }
        }
        // resize matrix
        let mut final_distance_cost_matrix: Vec<Vec<f32>> =
            infinity_matrix(self.state_m + 1, self.state_n);
        for row_index in 0..=self.state_m {
            for col_index in 1..=self.state_n {
                final_distance_cost_matrix[row_index][col_index - 1] =
                    distance_cost_matrix[row_index][col_index];
            }
        }
        let similarity = final_distance_cost_matrix[self.state_m - 1][self.state_n - 1];
        self.distance_cost_matrix = Option::Some(final_distance_cost_matrix);
        self.state_similarity = Option::Some(similarity);
        similarity
    }
    /// Allocation-free banded DTW cost matching `compute_optimal_path_with_window`.
    pub fn compute_optimal_path_with_window_cost(
        &mut self,
        first_sequence: &[T],
        second_sequence: &[T],
        w: u16,
    ) -> f32 {
        self.state_m = first_sequence.len();
        self.state_n = second_sequence.len();
        let similarity = compute_window_cost_with_scratch(
            first_sequence,
            second_sequence,
            w,
            self.distance_fn,
            &mut self.work_prev,
            &mut self.work_curr,
        );
        self.state_similarity = Some(similarity);
        similarity
    }
    pub fn retrieve_optimal_path(&self) -> Option<Vec<[usize; 2]>> {
        self.distance_cost_matrix.as_ref()?;
        let distance_cost_matrix = self.distance_cost_matrix.as_ref().unwrap();
        let mut row_index = self.state_m - 1;
        let mut column_index = self.state_n - 1;
        let mut path = vec![[0usize; 2]; cmp::min(row_index, column_index)];
        while (row_index > 0) || (column_index > 0) {
            if row_index > 0 && column_index > 0 {
                let insertion = distance_cost_matrix[row_index - 1][column_index];
                let deletion = distance_cost_matrix[row_index][column_index - 1];
                let matches = distance_cost_matrix[row_index - 1][column_index - 1];
                let min_value = [insertion, deletion, matches]
                    .iter()
                    .fold(f32::INFINITY, |a, &b| a.min(b));
                if min_value == matches {
                    row_index -= 1;
                    column_index -= 1;
                } else if min_value == insertion {
                    row_index -= 1;
                } else if min_value == deletion {
                    column_index -= 1;
                }
            } else if row_index > 0 && column_index == 0 {
                row_index -= 1;
            } else if row_index == 0 && column_index > 0 {
                column_index -= 1;
            }
            let part = [row_index, column_index];
            path.push(part);
        }
        path.reverse();
        Some(path)
    }
    pub fn new(distance_fn: fn(T, T) -> f32) -> Dtw<T> {
        Dtw {
            state_m: 0,
            state_n: 0,
            distance_fn,
            state_similarity: Option::None,
            distance_cost_matrix: Option::None,
            work_prev: Vec::new(),
            work_curr: Vec::new(),
        }
    }
}
fn infinity_matrix(rows: usize, cols: usize) -> Vec<Vec<f32>> {
    vec![vec![f32::INFINITY; cols]; rows]
}
fn abs_diff(a: usize, b: usize) -> usize {
    if a > b {
        a - b
    } else {
        b - a
    }
}

/// Allocation-free banded DTW using caller-owned rolling row buffers.
pub fn compute_window_cost_with_scratch<T: Copy>(
    first_sequence: &[T],
    second_sequence: &[T],
    w: u16,
    distance_fn: fn(T, T) -> f32,
    work_prev: &mut Vec<f32>,
    work_curr: &mut Vec<f32>,
) -> f32 {
    let m = first_sequence.len();
    let n = second_sequence.len();
    if m <= 1 || n == 0 {
        return f32::INFINITY;
    }
    let window = cmp::max(w as usize, abs_diff(m, n));
    let needed = n + 1;
    if work_prev.len() < needed {
        work_prev.resize(needed, f32::INFINITY);
    }
    if work_curr.len() < needed {
        work_curr.resize(needed, f32::INFINITY);
    }
    work_prev[..needed].fill(f32::INFINITY);
    work_prev[0] = 0.;

    for row_index in 1..m {
        work_curr[..needed].fill(f32::INFINITY);
        let start_index = if row_index > window {
            cmp::max(1, row_index - window)
        } else {
            1
        };
        let end_index = cmp::min(n + 1, row_index + window);
        let first_item = first_sequence[row_index - 1];
        for column_index in start_index..end_index {
            let cost = distance_fn(first_item, second_sequence[column_index - 1]);
            let insertion = work_prev[column_index];
            let deletion = work_curr[column_index - 1];
            let matches = work_prev[column_index - 1];
            work_curr[column_index] = cost + insertion.min(deletion).min(matches);
        }
        std::mem::swap(work_prev, work_curr);
    }

    work_prev[n]
}

/// Allocation-free banded DTW against a template row matrix and a flattened query buffer.
pub fn compute_window_cost_flat(
    first: &[Vec<f32>],
    second_flat: &[f32],
    n: usize,
    dim: usize,
    w: u16,
    distance_fn: fn(&[f32], &[f32]) -> f32,
    work_prev: &mut Vec<f32>,
    work_curr: &mut Vec<f32>,
) -> f32 {
    let m = first.len();
    if m <= 1 || n == 0 || dim == 0 {
        return f32::INFINITY;
    }
    let window = cmp::max(w as usize, abs_diff(m, n));
    let needed = n + 1;
    if work_prev.len() < needed {
        work_prev.resize(needed, f32::INFINITY);
    }
    if work_curr.len() < needed {
        work_curr.resize(needed, f32::INFINITY);
    }
    work_prev[..needed].fill(f32::INFINITY);
    work_prev[0] = 0.;

    for row_index in 1..m {
        work_curr[..needed].fill(f32::INFINITY);
        let start_index = if row_index > window {
            cmp::max(1, row_index - window)
        } else {
            1
        };
        let end_index = cmp::min(n + 1, row_index + window);
        let first_item = first[row_index - 1].as_slice();
        for column_index in start_index..end_index {
            let s_off = (column_index - 1) * dim;
            let second_item = &second_flat[s_off..s_off + dim];
            let cost = distance_fn(first_item, second_item);
            let insertion = work_prev[column_index];
            let deletion = work_curr[column_index - 1];
            let matches = work_prev[column_index - 1];
            work_curr[column_index] = cost + insertion.min(deletion).min(matches);
        }
        std::mem::swap(work_prev, work_curr);
    }

    work_prev[n]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff_cost(a: i32, b: i32) -> f32 {
        (a - b).abs() as f32
    }

    #[test]
    fn fast_window_cost_matches_full_matrix() {
        let a: Vec<i32> = (0..64).map(|i| (i * 7) % 19).collect();
        let b: Vec<i32> = (0..58).map(|i| (i * 5 + 3) % 17).collect();
        let mut dtw_ref = Dtw::new(diff_cost);
        let mut dtw_fast = Dtw::new(diff_cost);
        for band in [3, 5, 10] {
            let expected = dtw_ref.compute_optimal_path_with_window(&a, &b, band);
            let actual = dtw_fast.compute_optimal_path_with_window_cost(&a, &b, band);
            assert!(
                (expected - actual).abs() < 1e-6,
                "band={band}: expected {expected}, got {actual}"
            );
        }
    }
}
