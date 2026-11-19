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

/// Whether one more column of `cost` fits the slice (issue #18239). `spent` is
/// what the columns already in flight (or already admitted) cost, `budget` the
/// slice for all workers, `None` until the mesh cost estimate has warmed up —
/// then nothing is priced and this says yes, exactly the room check before it.
/// The first column always goes in: one dearer than the whole slice would
/// otherwise wait for a room it can never get, and the queue would never drain.
pub(super) fn fits(spent: u64, cost: u64, admitted: usize, budget: Option<u64>) -> bool {
    let Some(budget) = budget else {
        return true;
    };
    admitted == 0 || spent.saturating_add(cost) <= budget
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

    /// The cost slice bounds a tick's meshing without ever stopping it: cold
    /// statistics price nothing, the first column always goes in, and what the
    /// columns in flight have spent is what decides the next.
    #[test]
    fn a_column_fits_the_slice_or_waits_for_the_next_tick() {
        // No estimate yet: nothing is priced, so nothing is refused.
        assert!(fits(9_000_000, 9_000_000, 6, None));
        // The first column goes in however dear it is.
        assert!(fits(0, u64::MAX / 2, 0, Some(12_500_000)));
        assert!(fits(50_000_000, 30_000_000, 1, Some(87_500_000)));
        assert!(!fits(60_000_000, 30_000_000, 2, Some(87_500_000)));
        // Saturated, not wrapped: a spent past the budget refuses.
        assert!(!fits(u64::MAX, 30_000_000, 2, Some(87_500_000)));
    }

    /// A model of the mesh stage, not a run of it (#18239): seven workers, a
    /// 20 Hz tick, and forty columns queued, each one section costing a worker
    /// 30 ms. Admission by count — the `room` above — hands seven columns out
    /// in the first tick, so all seven workers sit inside a heavy job at once.
    /// Admission by cost spends a quarter of a tick per worker, so fewer heavy
    /// jobs start at a time; what must not regress is the nearest column still
    /// starting in the first tick.
    #[test]
    fn a_cost_slice_keeps_the_workers_off_seven_heavy_jobs_at_once() {
        // (milliseconds with all seven workers meshing, tick the nearest column
        // started in).
        fn model(by_cost: bool) -> (usize, u64) {
            const WORKERS: usize = 7;
            const MS: u64 = 1_000_000;
            const TICK: u64 = 50 * MS;
            const JOB: u64 = 30 * MS;
            let budget = by_cost.then_some(WORKERS as u64 * MESH_SLICE_SHARE_NANOS);
            let mut free_at = [0u64; WORKERS];
            let mut queued = 40;
            let (mut all_seven, mut first_started) = (0, u64::MAX);
            for tick in 0..40 {
                let start = tick * TICK;
                let idle: Vec<usize> = (0..WORKERS).filter(|&w| free_at[w] <= start).collect();
                let mut admitted = 0;
                for w in idle {
                    let spent = free_at.iter().filter(|&&at| at > start).count() as u64 * JOB;
                    if queued > 0 && fits(spent, JOB, admitted, budget) {
                        free_at[w] = start + JOB;
                        queued -= 1;
                        admitted += 1;
                        first_started = first_started.min(start);
                    }
                }
                for now in (start..start + TICK).step_by(MS as usize) {
                    if free_at.iter().all(|&at| at > now) {
                        all_seven += 1;
                    }
                }
            }
            (all_seven, first_started / TICK)
        }

        let (count_all_seven, count_start) = model(false);
        let (cost_all_seven, cost_start) = model(true);
        assert_eq!(count_start, 0);
        assert_eq!(
            cost_start, 0,
            "the nearest column still starts in the first tick"
        );
        assert!(
            cost_all_seven * 4 <= count_all_seven,
            "{cost_all_seven} vs {count_all_seven} ms"
        );
    }
}
