use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
pub const KINDS: [&str; 7] = [
    "load", "generate", "decorate", "finish", "light", "mesh", "other",
];
/// The slot of `KINDS` the mesh job holds: the one the admission check prices.
pub(super) const MESH_KIND: usize = 5;
/// Samples the mesh estimate needs before the admission check trusts it; a job
/// that costs a millisecond and more settles in a few.
const MESH_WARMUP: u64 = 4;
/// The estimate's step: an eighth of the way to a sample, so twenty samples
/// follow a step in cost to within a few percent.
const MESH_EWMA_SHIFT: u32 = 3;
static ENABLED: AtomicBool = AtomicBool::new(false);
static NANOS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
static COUNT: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
static MESH_EWMA: AtomicU64 = AtomicU64::new(0);
static MESH_SAMPLES: AtomicU64 = AtomicU64::new(0);
pub fn enable(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}
pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub(super) fn add(kind: usize, nanos: u64) {
    NANOS[kind].fetch_add(nanos, Ordering::Relaxed);
    COUNT[kind].fetch_add(1, Ordering::Relaxed);
}
/// (kind, nanoseconds, count) since the last call; resets them.
pub fn take() -> Vec<(&'static str, u64, u64)> {
    (0..KINDS.len())
        .map(|i| {
            (
                KINDS[i],
                NANOS[i].swap(0, Ordering::Relaxed),
                COUNT[i].swap(0, Ordering::Relaxed),
            )
        })
        .collect()
}
/// One EWMA step: an eighth of the way from `prev` to `sample`. In i64 so a
/// cheaper sample pulls the estimate down as fast as a dearer one lifts it;
/// u64's shift of a subtraction could only ever grow it.
pub(super) fn ewma_step(prev: u64, sample: u64) -> u64 {
    let (prev, sample) = (prev as i64, sample as i64);
    (prev + ((sample - prev) >> MESH_EWMA_SHIFT)).max(0) as u64
}
/// What a sample makes of the estimate: the first one seeds it, rather than
/// creeping up from zero an eighth at a time; the rest step toward it.
pub(super) fn ewma_next(prev: u64, sample: u64, samples: u64) -> u64 {
    if samples == 0 {
        sample
    } else {
        ewma_step(prev, sample)
    }
}
/// The estimate as the admission check reads it: `None` while it is cold or
/// zero, so a statistic that has seen nothing bounds nothing.
pub(super) fn cost_ready(samples: u64, cost: u64) -> Option<u64> {
    (samples >= MESH_WARMUP && cost > 0).then_some(cost)
}
/// Feed one finished mesh job's cost to the estimate. Every worker calls this,
/// so it is two relaxed atomics and a sample lost to a race costs one step of
/// 1/8 — the estimate is a hint for the admission check, not a measurement.
pub(super) fn observe_mesh(nanos: u64) {
    let samples = MESH_SAMPLES.load(Ordering::Relaxed);
    let prev = MESH_EWMA.load(Ordering::Relaxed);
    MESH_EWMA.store(ewma_next(prev, nanos, samples), Ordering::Relaxed);
    MESH_SAMPLES.fetch_add(1, Ordering::Relaxed);
}
/// What one mesh job costs, once the estimate has warmed up; `None` until then.
pub fn mesh_cost_ns() -> Option<u64> {
    cost_ready(
        MESH_SAMPLES.load(Ordering::Relaxed),
        MESH_EWMA.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The estimate follows the cost both ways and has caught up with a step
    /// after about twenty samples; the first sample seeds it rather than
    /// creeping up from zero an eighth at a time.
    #[test]
    fn the_mesh_estimate_follows_the_cost_it_is_fed() {
        assert_eq!(ewma_next(0, 30_000_000, 0), 30_000_000);
        assert!(ewma_step(0, 30_000_000) < 30_000_000);
        // Seeded at 5 ms by a first, cheap job, then twenty 30 ms ones.
        let mut cost = ewma_next(0, 5_000_000, 0);
        for _ in 0..20 {
            cost = ewma_step(cost, 30_000_000);
        }
        assert!((25_000_000..=30_000_000).contains(&cost), "{cost}");
        // A cheaper job pulls the estimate down as fast as a dearer lifts it.
        assert!(ewma_step(cost, 10_000_000) < cost);
        assert!(ewma_step(cost, 60_000_000) > cost);
    }

    /// A cold estimate is no estimate at all: the admission check reading it as
    /// a cost of zero would refuse nothing, but reading zero as a warmed-up cost
    /// would refuse every column past the first.
    #[test]
    fn a_cold_estimate_is_no_estimate_at_all() {
        assert_eq!(cost_ready(0, 0), None);
        assert_eq!(cost_ready(MESH_WARMUP - 1, 30_000_000), None);
        assert_eq!(cost_ready(MESH_WARMUP, 0), None);
        assert_eq!(cost_ready(MESH_WARMUP, 30_000_000), Some(30_000_000));
    }
}
