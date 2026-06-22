//! Order-preserving parallel map.
//!
//! The package matcher's dominant cost is the per-source oxc parse + fingerprint
//! (`package_source_fingerprint`), run independently for every corpus source in
//! several passes. These loops are embarrassingly parallel but were written
//! single-threaded. [`par_map`] fans them out across cores while concatenating
//! results in input order, so every downstream map/index it feeds stays
//! byte-identical to the previous single-threaded build (determinism is a hard
//! requirement for reproducible matching).

/// Map `f` over `items` across all available cores, returning results in the
/// same order as `items`. Falls back to a plain sequential map for tiny inputs
/// where thread setup would dominate.
pub(crate) fn par_map<'a, T, R, F>(items: &'a [T], f: F) -> Vec<R>
where
    T: Sync + 'a,
    R: Send,
    F: Fn(&'a T) -> R + Sync,
{
    let len = items.len();
    if len == 0 {
        return Vec::new();
    }
    let thread_count = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(len);
    if thread_count <= 1 || len < 16 {
        return items.iter().map(&f).collect();
    }
    let chunk_size = len.div_ceil(thread_count).max(1);
    let f = &f;
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(move || chunk.iter().map(f).collect::<Vec<R>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("par_map worker panicked"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::par_map;

    #[test]
    fn preserves_order_and_maps_all() {
        let input: Vec<usize> = (0..1000).collect();
        let output = par_map(&input, |n| n * 2);
        assert_eq!(output.len(), 1000);
        for (index, value) in output.iter().enumerate() {
            assert_eq!(*value, index * 2);
        }
    }

    #[test]
    fn empty_and_small_inputs() {
        assert_eq!(par_map::<i32, i32, _>(&[], |n| *n), Vec::<i32>::new());
        assert_eq!(par_map(&[1, 2, 3], |n| n + 1), vec![2, 3, 4]);
    }
}
