//! Bounded submission: the light and mesh stages hand the workers only the
//! nearest of what is ready.

/// The nearest `room` of the `candidates`, ordered: the light and mesh
/// stages submit bounded work, chosen from everything that is ready each
/// `update`, so what runs is always the nearest of what is left — chosen
/// again, from the current centre, as capacity frees, the way vanilla's
/// chunk task queues follow the current ticket levels. The selection is
/// O(n); the sort orders only what is submitted.
pub(super) fn nearest_first<T: Ord>(candidates: &mut Vec<T>, room: usize) {
    if candidates.len() > room {
        candidates.select_nth_unstable(room);
        candidates.truncate(room);
    }
    candidates.sort_unstable();
}

/// The mesh time one worker's columns may take in a tick: a quarter of a 20 Hz
/// tick (Minecraft's 50 ms), so the pool starts a few heavy columns per tick
/// rather than every idle worker's at once, and a heavy column's tail does not
/// run under the next tick's light and generate jobs.
pub(super) const MESH_SLICE_SHARE_NANOS: u64 = 12_500_000;

/// Whether one more column of `cost` fits what is left of the mesh slice (issue
/// #18239). `left` is the slice for all workers less what the columns in flight
/// already cost — `None` until the mesh cost estimate has warmed up, when nothing
/// is priced and this says yes, exactly the room check before it. `start_anyway`
/// is a pool with nothing in flight: one column dearer than the whole slice would
/// otherwise wait for room it can never have, and the queue would never drain.
pub(super) fn fits(left: Option<u64>, cost: u64, start_anyway: bool) -> bool {
    left.is_none_or(|left| start_anyway || cost <= left)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The light and mesh stages submit bounded work, the nearest first:
    /// what exceeds the room stays dirty, to be chosen again — from the
    /// current centre — as capacity frees. Submitted whole, the queue once
    /// held every ready column's jobs and grew with the render distance
    /// (#3629); ties break by position, so the choice is deterministic.
    #[test]
    fn bounded_submission_takes_the_nearest_first() {
        let candidates: Vec<(i64, i32, i32, u32)> = (0..40i32)
            .map(|i| {
                let (x, z) = (i / 8 - 2, i % 8 - 3);
                (
                    i64::from((x - 3).pow(2) + (z - 2).pow(2)),
                    x,
                    z,
                    1 << (i % 24),
                )
            })
            .collect();
        let mut expected = candidates.clone();
        expected.sort_unstable();
        let mut none = candidates.clone();
        nearest_first(&mut none, 0);
        assert!(none.is_empty());
        let mut nearest = candidates;
        nearest_first(&mut nearest, 5);
        assert_eq!(nearest, expected[..5]);
        let mut past = expected.clone();
        nearest_first(&mut past, expected.len() + 10);
        assert_eq!(past, expected);
    }

    /// The slice prices what is in flight: a column fits what is left of it, a
    /// cold estimate prices nothing, and the first column into an empty pool goes
    /// in whatever it costs.
    #[test]
    fn a_column_fits_what_is_left_of_the_slice() {
        assert!(
            fits(None, u64::MAX / 2, false),
            "cold statistics price nothing"
        );
        assert!(
            fits(Some(0), u64::MAX / 2, true),
            "an empty pool starts one anyway"
        );
        assert!(fits(Some(40_000_000), 30_000_000, false));
        assert!(fits(Some(30_000_000), 30_000_000, false), "fits exactly");
        assert!(!fits(Some(25_000_000), 30_000_000, false));
    }
}
