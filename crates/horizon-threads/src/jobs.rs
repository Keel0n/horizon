//! One pool of worker threads for the whole process, shared by every source of
//! background work: chunk generation, light and meshing (the client's and each
//! of the server thread's levels), chunk encoding, far terrain.
//!
//! Before it, each of those started its own threads, about one per core each:
//! a single player world ran four or five such pools at once, ~100 threads on a
//! 32-thread CPU, and they fought each other and the render and server threads
//! for cores. Here the threads are counted once, for the machine
//! ([`configure`]), and the work goes to them in the order it matters.
//!
//! A [`Source`] keeps its own queue (it can rank its jobs again when the player
//! moves, or drop the ones that left the view); a worker that is free asks every
//! source for the [`Rank`] of its next job and runs the most urgent one: the
//! [`Class`] first, then the source's key (a chunk's distance from the player).
//! Every free worker takes the next job as soon as it is free, so no core waits
//! while work is queued, whichever source queued it. Jobs are coarse (a chunk, a
//! section, a tile: a millisecond or more), so one lock for the choice costs
//! nothing next to them.
//!
//! A worker asks the sources for their next rank under the pool's read lock
//! of its sources and nothing else, so a source may call [`Pool::notify`]
//! anywhere, its queue's lock held or not; a notify never waits for a worker.
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// How urgent a job is, before its source's key: every queued job of a lower
/// class runs before any of a higher one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// Work something waits for now: saves, a tick's chunk encoding.
    Urgent,
    /// The chunks around the player: generation, light, meshing.
    Near,
    /// Far terrain and the meshing of levels the player only views: runs on
    /// what the near work leaves, and never on every worker at once, so near
    /// work that arrives finds a worker free.
    Far,
}

/// A job's place in the pool's order: lower runs first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rank {
    pub class: Class,
    /// The source's own order within the class (a column's distance).
    pub key: i64,
}

impl Rank {
    pub const fn new(class: Class, key: i64) -> Self {
        Self { class, key }
    }
}

/// Work queued for the pool.
pub trait Source: Send + Sync {
    /// The rank of the job [`Source::run`] would take next, or None while
    /// nothing is queued. Called under the pool's lock: cheap, no blocking.
    fn next(&self) -> Option<Rank>;
    /// Take the next job and run it on worker `worker` (0..[`Pool::threads`]).
    /// False if there was none after all.
    fn run(&self, worker: usize) -> bool;
    /// Worker `worker` has waited [`IDLE_RELEASE`] with no work: free what it
    /// keeps between jobs.
    fn idle(&self, _worker: usize) {}
    /// Every worker is idle: free what the workers share.
    fn all_idle(&self) {}
    /// What a heap-census build tags its jobs' allocations (`crate::heap_tag`).
    fn label(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

/// How long a worker waits with no job before it frees its buffers.
pub const IDLE_RELEASE: Duration = Duration::from_secs(5);

/// Per-worker state for a source: worker `i` only ever touches slot `i`, so
/// the locks are never contended. The source owns it, so it goes with the source.
pub struct PerWorker<T> {
    slots: Box<[Mutex<Option<T>>]>,
}

impl<T> PerWorker<T> {
    pub fn new(workers: usize) -> Self {
        Self {
            slots: (0..workers.max(1)).map(|_| Mutex::new(None)).collect(),
        }
    }
    /// `f` with worker `worker`'s state, made by `init` the first time.
    pub fn with<R>(
        &self,
        worker: usize,
        init: impl FnOnce() -> T,
        f: impl FnOnce(&mut T) -> R,
    ) -> R {
        let mut slot = lock(&self.slots[worker % self.slots.len()]);
        f(slot.get_or_insert_with(init))
    }
    /// `f` with worker `worker`'s state if it has one.
    pub fn with_existing<R>(&self, worker: usize, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        lock(&self.slots[worker % self.slots.len()]).as_mut().map(f)
    }
    /// Drop worker `worker`'s state.
    pub fn clear(&self, worker: usize) -> Option<T> {
        lock(&self.slots[worker % self.slots.len()]).take()
    }
}

/// One counter on a cache line of its own (as [`crate::alloc_count`]'s slots are):
/// these are written by every worker on every job, and two of them within one
/// 64-byte line make the cores that own the others re-read it for nothing
/// (false sharing). Padding is per field, never by position: rustc's field
/// order is not a contract.
#[repr(align(64))]
struct Padded<T>(T);

impl<T> Padded<T> {
    const fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> std::ops::Deref for Padded<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

struct Entry {
    id: u64,
    source: Arc<dyn Source>,
    /// Workers running one of its jobs.
    running: Padded<AtomicUsize>,
    /// Taken out of the pool: the worker that ends its last job tells
    /// [`Registration`]'s drop, which waits for it.
    retired: AtomicBool,
    /// When one of its jobs last ended (or it was registered).
    last_job: Mutex<Instant>,
    /// It was told it is idle ([`release_quiet_sources`]) since that job.
    quiet: Padded<AtomicBool>,
}

struct Inner {
    /// Read by every choice, written only to add or take out a source.
    sources: RwLock<Vec<Arc<Entry>>>,
    next_id: AtomicU64,
    /// Bumped by every [`Pool::notify`]: a worker that found nothing sleeps
    /// until it changes, so a job queued while it looked is not missed. A
    /// notify is two atomic operations while every worker is busy, and takes
    /// `sleep` (held by no source code) only to wake one: the thread queuing
    /// work (a frame, a tick) never waits behind a worker's choice.
    epoch: Padded<AtomicU64>,
    sleepers: Padded<AtomicUsize>,
    sleep: Mutex<()>,
    wake: Condvar,
    /// Signalled when a retired source's job ends.
    finished: Condvar,
    /// Workers running a [`Class::Far`] job.
    far: Padded<AtomicUsize>,
    /// Workers that freed their buffers and took no job since.
    released: Padded<AtomicUsize>,
    threads: Padded<AtomicUsize>,
    idle_release: Duration,
}

/// The pool: [`pool`] for the process's own, [`Pool::new`] for a test's.
pub struct Pool {
    inner: Arc<Inner>,
    threads: usize,
}

/// A source's place in the pool; dropping it takes the source out and waits
/// for the jobs of it that are running (so never drop it in one of its own jobs).
pub struct Registration {
    inner: Arc<Inner>,
    id: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic in a job is caught; the pool's own state is never left half
    // changed, so a poisoned lock is still consistent.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

static CONFIGURED: AtomicUsize = AtomicUsize::new(0);
static POOL: OnceLock<Pool> = OnceLock::new();

/// The number of workers the machine gets by default: one thread per core
/// but one ([`crate::default_workers`]: per performance core on Apple silicon), which the
/// render and server threads share with them (they run ahead of the workers:
/// [`crate::favour_current`], [`crate::behind_server`]).
pub fn default_threads() -> usize {
    crate::default_workers()
}

/// Size the process's pool before its first use (later calls change
/// nothing): `threads` workers, or `HORIZON_JOB_THREADS` when it is set.
pub fn configure(threads: usize) {
    CONFIGURED.store(threads.max(1), Ordering::Relaxed);
}

/// The process's pool, started on first use.
pub fn pool() -> &'static Pool {
    POOL.get_or_init(|| {
        let threads = std::env::var("HORIZON_JOB_THREADS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or_else(|| match CONFIGURED.load(Ordering::Relaxed) {
                0 => default_threads(),
                n => n,
            });
        // The name the crash reports know a worker by (vanilla's `Util` workers).
        Pool::new(threads, "horizon-worker")
    })
}

impl Pool {
    /// A pool of `threads` workers named `name-<i>`. Workers that the system
    /// refuses to start leave it smaller ([`Pool::threads`] counts those that started).
    pub fn new(threads: usize, name: &str) -> Self {
        Self::with_idle_release(threads, name, IDLE_RELEASE)
    }

    /// [`Pool::new`] whose workers free their buffers after `idle_release`.
    pub fn with_idle_release(threads: usize, name: &str, idle_release: Duration) -> Self {
        Self::with_spawner(threads, idle_release, |worker, i| {
            horizon_guest_stack::builder()
                .name(format!("{name}-{i}"))
                .spawn(move || {
                    crate::behind_server();
                    work(&worker, i);
                })
                .map(|_| ())
        })
    }

    fn with_spawner(
        threads: usize,
        idle_release: Duration,
        mut spawn: impl FnMut(Arc<Inner>, usize) -> std::io::Result<()>,
    ) -> Self {
        let inner = Arc::new(Inner {
            sources: RwLock::new(Vec::new()),
            next_id: AtomicU64::new(0),
            epoch: Padded::new(AtomicU64::new(0)),
            sleepers: Padded::new(AtomicUsize::new(0)),
            sleep: Mutex::new(()),
            wake: Condvar::new(),
            finished: Condvar::new(),
            far: Padded::new(AtomicUsize::new(0)),
            released: Padded::new(AtomicUsize::new(0)),
            threads: Padded::new(AtomicUsize::new(threads.max(1))),
            idle_release,
        });
        let mut started = 0;
        let mut last_error = None;
        for i in 0..threads.max(1) {
            match spawn(inner.clone(), i) {
                Ok(()) => started += 1,
                Err(error) => last_error = Some(error),
            }
        }
        assert!(
            started > 0,
            "GuestWorkerUnavailable: no job worker could start: {last_error:?}"
        );
        inner.threads.store(started, Ordering::Relaxed);
        Self {
            inner,
            threads: started,
        }
    }

    /// The workers: the size of a source's [`PerWorker`] and of the work it
    /// keeps queued.
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Add a source; its jobs run until the registration is dropped.
    pub fn register(&self, source: Arc<dyn Source>) -> Registration {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner
            .sources
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(Entry {
                id,
                source,
                running: Padded::new(AtomicUsize::new(0)),
                retired: AtomicBool::new(false),
                last_job: Mutex::new(Instant::now()),
                quiet: Padded::new(AtomicBool::new(false)),
            }));
        self.inner.notify(true);
        Registration {
            inner: self.inner.clone(),
            id,
        }
    }

    /// Wake a worker for a job just queued (after the queue's lock is released).
    pub fn notify(&self) {
        self.inner.notify(false);
    }

    /// Wake every worker: many jobs were queued at once.
    pub fn notify_all(&self) {
        self.inner.notify(true);
    }
}

impl Inner {
    fn notify(&self, all: bool) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if self.sleepers.load(Ordering::SeqCst) > 0 {
            // A worker between counting itself asleep and waiting holds
            // `sleep`: taking it here means it is waiting when told.
            drop(lock(&self.sleep));
            if all {
                self.wake.notify_all();
            } else {
                self.wake.notify_one();
            }
        }
    }
}

impl Registration {
    /// Wake a worker for a job just queued: [`Pool::notify`].
    pub fn notify(&self) {
        self.inner.notify(false);
    }
    pub fn notify_all(&self) {
        self.inner.notify(true);
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let entry = {
            let mut sources = self
                .inner
                .sources
                .write()
                .unwrap_or_else(|e| e.into_inner());
            let Some(at) = sources.iter().position(|e| e.id == self.id) else {
                return;
            };
            sources.swap_remove(at)
        };
        // No worker takes a job of it now (a worker counts itself running
        // while it holds the sources' read lock); wait for those that did.
        entry.retired.store(true, Ordering::SeqCst);
        let mut sleep = lock(&self.inner.sleep);
        while entry.running.load(Ordering::SeqCst) > 0 {
            sleep = self
                .inner
                .finished
                .wait(sleep)
                .unwrap_or_else(|e| e.into_inner());
        }
        drop(sleep);
        // The source's last reference may go here, outside every lock.
        drop(entry);
    }
}

/// The most urgent queued job's source, counted as running, if any may run now.
fn choose(inner: &Inner) -> Option<(Arc<Entry>, bool)> {
    let threads = inner.threads.load(Ordering::Relaxed);
    // Far work keeps one worker free for near work (when there are two or more).
    let far_cap = threads.saturating_sub(1).max(1);
    let sources = inner.sources.read().unwrap_or_else(|e| e.into_inner());
    let mut skip_far = false;
    loop {
        let far_open = !skip_far && inner.far.load(Ordering::SeqCst) < far_cap;
        let mut best: Option<(&Arc<Entry>, Rank)> = None;
        for entry in sources.iter() {
            let Some(rank) = entry.source.next() else {
                continue;
            };
            if rank.class == Class::Far && !far_open {
                continue;
            }
            if best.is_none_or(|(_, b)| rank < b) {
                best = Some((entry, rank));
            }
        }
        let (entry, rank) = best?;
        let far = rank.class == Class::Far;
        if far && !take_far_place(inner, far_cap) {
            // Another worker took the last far place meanwhile.
            skip_far = true;
            continue;
        }
        entry.running.fetch_add(1, Ordering::SeqCst);
        return Some((entry.clone(), far));
    }
}

/// Tell each source that has had no job for the idle delay, while others keep
/// the workers busy, that it is idle: its state in every worker
/// ([`Source::idle`]) and what its workers share ([`Source::all_idle`]). The
/// pool's own release needs every worker idle at once, which a client that
/// keeps meshing after its world loaded never is, while its server's
/// generator holds buffers and caches nothing reads until the player moves on.
fn release_quiet_sources(inner: &Inner) {
    let quiet: Vec<Arc<Entry>> = inner
        .sources
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|e| {
            !e.quiet.load(Ordering::SeqCst)
                && e.running.load(Ordering::SeqCst) == 0
                && lock(&e.last_job).elapsed() >= inner.idle_release
                && e.source.next().is_none()
        })
        .cloned()
        .collect();
    let mut freed = false;
    for entry in quiet {
        if entry.quiet.swap(true, Ordering::SeqCst) {
            continue;
        }
        for worker in 0..inner.threads.load(Ordering::Relaxed) {
            let _ = catch_unwind(AssertUnwindSafe(|| entry.source.idle(worker)));
        }
        let _ = catch_unwind(AssertUnwindSafe(|| entry.source.all_idle()));
        freed = true;
    }
    if freed {
        crate::release_free_memory();
    }
}

/// One more far job running, if fewer than `cap` are.
fn take_far_place(inner: &Inner, cap: usize) -> bool {
    let mut n = inner.far.load(Ordering::SeqCst);
    while n < cap {
        match inner
            .far
            .compare_exchange_weak(n, n + 1, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => return true,
            Err(now) => n = now,
        }
    }
    false
}

fn work(inner: &Inner, worker: usize) {
    let mut released = false;
    // When this worker last ran a job. A wake that finds no job for it (another
    // worker took it, or a source woke the pool for nothing) doesn't restart its
    // wait: with jobs queued every few seconds on some worker, a pool of idle
    // workers would otherwise never free anything.
    let mut idle_since = Instant::now();
    let mut release_seen = crate::release_seen();
    loop {
        let epoch = inner.epoch.load(Ordering::SeqCst);
        if let Some((entry, far)) = choose(inner) {
            if released {
                inner.released.fetch_sub(1, Ordering::Relaxed);
                released = false;
            }
            // A panicking job loses its own work, not the worker: the panic
            // hook has already reported it.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let _tag =
                    crate::HEAP_CENSUS.then(|| crate::heap_census::tag(entry.source.label()));
                entry.source.run(worker)
            }));
            if far {
                inner.far.fetch_sub(1, Ordering::SeqCst);
            }
            *lock(&entry.last_job) = Instant::now();
            entry.quiet.store(false, Ordering::SeqCst);
            entry.running.fetch_sub(1, Ordering::SeqCst);
            if entry.retired.load(Ordering::SeqCst) {
                drop(lock(&inner.sleep));
                inner.finished.notify_all();
            }
            idle_since = Instant::now();
            release_quiet_sources(inner);
            // Asked to give memory back while jobs keep this worker busy (no
            // gap for the timed checks below): do it between two jobs.
            if crate::release_asked(&mut release_seen) {
                crate::release_free_memory();
            }
            continue;
        }
        let sleep = lock(&inner.sleep);
        inner.sleepers.fetch_add(1, Ordering::SeqCst);
        let still = |_: &mut ()| inner.epoch.load(Ordering::SeqCst) == epoch;
        let timed_out = if released {
            // Already released: wake only for a job, or to give back what
            // other threads freed into this worker's heap since
            // (`release_everywhere`).
            let (sleep, wait) = inner
                .wake
                .wait_timeout_while(sleep, crate::RELEASE_POLL, still)
                .unwrap_or_else(|e| e.into_inner());
            drop(sleep);
            if wait.timed_out() && crate::release_asked(&mut release_seen) {
                inner.sleepers.fetch_sub(1, Ordering::SeqCst);
                crate::release_free_memory();
                continue;
            }
            false
        } else {
            let left = inner.idle_release.saturating_sub(idle_since.elapsed());
            let (sleep, wait) = inner
                .wake
                .wait_timeout_while(sleep, left, still)
                .unwrap_or_else(|e| e.into_inner());
            drop(sleep);
            wait.timed_out()
        };
        inner.sleepers.fetch_sub(1, Ordering::SeqCst);
        if !timed_out {
            continue;
        }
        // Nothing to do for a while (a server nobody is on, a player standing
        // still): free this worker's buffers until the next job needs them.
        released = true;
        let last = inner.released.fetch_add(1, Ordering::Relaxed) + 1
            == inner.threads.load(Ordering::Relaxed);
        let sources: Vec<_> = inner
            .sources
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|e| e.source.clone())
            .collect();
        for source in &sources {
            let _ = catch_unwind(AssertUnwindSafe(|| source.idle(worker)));
        }
        if last {
            for source in &sources {
                let _ = catch_unwind(AssertUnwindSafe(|| source.all_idle()));
            }
        }
        // The allocator keeps freed pages until memory runs short: hand back
        // what this worker just freed (its own heap), and the shared arenas'
        // once every worker has.
        crate::release_asked(&mut release_seen);
        crate::release_free_memory();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic(expected = "GuestWorkerUnavailable")]
    fn all_worker_spawn_failures_refuse_the_pool() {
        super::Pool::with_spawner(2, super::IDLE_RELEASE, |_, _| {
            Err(std::io::Error::other("injected stack reservation refusal"))
        });
    }

    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::time::Instant;

    /// Jobs with fixed ranks; each records its name when it runs.
    struct Fixed {
        jobs: Mutex<VecDeque<(Rank, &'static str)>>,
        ran: mpsc::Sender<&'static str>,
        hold: Option<Arc<(Mutex<bool>, Condvar)>>,
    }
    impl Source for Fixed {
        fn next(&self) -> Option<Rank> {
            lock(&self.jobs).iter().map(|(r, _)| *r).min()
        }
        fn run(&self, _worker: usize) -> bool {
            let job = {
                let mut jobs = lock(&self.jobs);
                let Some(at) = jobs
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, (r, _))| *r)
                    .map(|(i, _)| i)
                else {
                    return false;
                };
                jobs.remove(at).unwrap()
            };
            if let Some(hold) = &self.hold {
                let (open, cv) = &**hold;
                let mut open = lock(open);
                while !*open {
                    open = cv.wait(open).unwrap();
                }
            }
            let _ = self.ran.send(job.1);
            true
        }
    }
    fn fixed(jobs: &[(Rank, &'static str)], ran: mpsc::Sender<&'static str>) -> Arc<Fixed> {
        Arc::new(Fixed {
            jobs: Mutex::new(jobs.iter().copied().collect()),
            ran,
            hold: None,
        })
    }
    fn recv_n(rx: &mpsc::Receiver<&'static str>, n: usize) -> Vec<&'static str> {
        (0..n)
            .map(|_| rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect()
    }

    #[test]
    fn one_worker_runs_every_source_in_rank_order() {
        let pool = Pool::new(1, "test-job");
        let (tx, rx) = mpsc::channel();
        // Held until both sources are in, so the order is the pool's choice.
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let first = Arc::new(Fixed {
            jobs: Mutex::new([(Rank::new(Class::Urgent, i64::MIN), "gate")].into()),
            ran: tx.clone(),
            hold: Some(gate.clone()),
        });
        let _g = pool.register(first);
        let a = fixed(
            &[
                (Rank::new(Class::Near, 5), "near5"),
                (Rank::new(Class::Far, 0), "far0"),
            ],
            tx.clone(),
        );
        let b = fixed(
            &[
                (Rank::new(Class::Near, 1), "near1"),
                (Rank::new(Class::Urgent, 9), "urgent9"),
            ],
            tx,
        );
        let _a = pool.register(a);
        let _b = pool.register(b);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert_eq!(
            recv_n(&rx, 5),
            ["gate", "urgent9", "near1", "near5", "far0"]
        );
    }

    #[test]
    fn far_work_leaves_a_worker_for_near_work() {
        let pool = Pool::new(3, "test-job");
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (tx, rx) = mpsc::channel();
        let far = Arc::new(Fixed {
            jobs: Mutex::new((0..6).map(|i| (Rank::new(Class::Far, i), "far")).collect()),
            ran: tx.clone(),
            hold: Some(gate.clone()),
        });
        let _far = pool.register(far);
        // Two far jobs hold two workers; the third stays free for this one.
        std::thread::sleep(Duration::from_millis(100));
        let near = fixed(&[(Rank::new(Class::Near, 0), "near")], tx);
        let near_reg = pool.register(near);
        near_reg.notify();
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), "near");
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert_eq!(recv_n(&rx, 6), ["far"; 6]);
    }

    /// A source that counts its jobs, queued from outside while the workers run.
    struct Counter {
        left: Mutex<u64>,
        done: AtomicUsize,
        per_worker: PerWorker<u64>,
        idle: AtomicBool,
        all_idle: AtomicBool,
    }
    impl Source for Counter {
        fn next(&self) -> Option<Rank> {
            (*lock(&self.left) > 0).then_some(Rank::new(Class::Near, 0))
        }
        fn run(&self, worker: usize) -> bool {
            {
                let mut left = lock(&self.left);
                if *left == 0 {
                    return false;
                }
                *left -= 1;
            }
            self.per_worker.with(worker, || 0, |n| *n += 1);
            self.done.fetch_add(1, Ordering::Relaxed);
            true
        }
        fn idle(&self, worker: usize) {
            self.per_worker.clear(worker);
            self.idle.store(true, Ordering::Relaxed);
        }
        fn all_idle(&self) {
            self.all_idle.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn no_job_is_missed_when_queued_while_workers_look() {
        let pool = Pool::new(4, "test-job");
        let counter = Arc::new(Counter {
            left: Mutex::new(0),
            done: AtomicUsize::new(0),
            per_worker: PerWorker::new(pool.threads()),
            idle: AtomicBool::new(false),
            all_idle: AtomicBool::new(false),
        });
        let reg = pool.register(counter.clone());
        for _ in 0..20_000 {
            *lock(&counter.left) += 1;
            reg.notify();
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while counter.done.load(Ordering::Relaxed) < 20_000 {
            assert!(Instant::now() < deadline, "jobs left behind");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A pool woken far more often than its idle delay, with no job for most of
    /// its workers (the game queues a few meshes a second long after loading):
    /// each worker still frees its state once it has run nothing for the delay,
    /// and the whole pool once none has.
    #[test]
    fn workers_woken_for_nothing_still_free_their_state() {
        let release = Duration::from_millis(300);
        let pool = Pool::with_idle_release(4, "test-job", release);
        let counter = Arc::new(Counter {
            left: Mutex::new(0),
            done: AtomicUsize::new(0),
            per_worker: PerWorker::new(pool.threads()),
            idle: AtomicBool::new(false),
            all_idle: AtomicBool::new(false),
        });
        let reg = pool.register(counter.clone());
        let deadline = Instant::now() + release * 10;
        while !counter.all_idle.load(Ordering::Relaxed) {
            assert!(
                Instant::now() < deadline,
                "woken every 50 ms, the workers never released"
            );
            reg.notify_all();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(counter.idle.load(Ordering::Relaxed));
    }

    /// A server asking for its memory back while a job arrives every 50 ms,
    /// shorter than any idle wait: the workers still give it back, between
    /// two jobs.
    #[test]
    fn a_busy_worker_still_gives_memory_back_when_asked() {
        let pool = Pool::with_idle_release(2, "test-busy-release", Duration::from_secs(60));
        let counter = Arc::new(Counter {
            left: Mutex::new(0),
            done: AtomicUsize::new(0),
            per_worker: PerWorker::new(pool.threads()),
            idle: AtomicBool::new(false),
            all_idle: AtomicBool::new(false),
        });
        let reg = pool.register(counter.clone());
        let by_worker = || {
            crate::RELEASED_BY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|name| name.starts_with("test-busy-release-"))
        };
        // A worker that has run a job has started counting releases; one
        // started after the request would rightly not see it.
        *lock(&counter.left) += 1;
        reg.notify();
        let deadline = Instant::now() + Duration::from_secs(4);
        while counter.done.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "no worker ran the first job");
            std::thread::sleep(Duration::from_millis(1));
        }
        crate::release_everywhere();
        let deadline = Instant::now() + Duration::from_secs(4);
        while !by_worker() {
            assert!(
                Instant::now() < deadline,
                "kept busy, no worker gave its memory back"
            );
            *lock(&counter.left) += 1;
            reg.notify();
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// One source idle while another keeps the workers busy (a client meshing
    /// beside its server's generator): the idle one is still told, once.
    #[test]
    fn a_quiet_source_is_released_while_another_keeps_the_workers_busy() {
        let release = Duration::from_millis(300);
        let pool = Pool::with_idle_release(2, "test-job", release);
        let counter = |left| {
            Arc::new(Counter {
                left: Mutex::new(left),
                done: AtomicUsize::new(0),
                per_worker: PerWorker::new(pool.threads()),
                idle: AtomicBool::new(false),
                all_idle: AtomicBool::new(false),
            })
        };
        let (quiet, busy) = (counter(4), counter(0));
        let quiet_reg = pool.register(quiet.clone());
        let busy_reg = pool.register(busy.clone());
        quiet_reg.notify_all();
        let deadline = Instant::now() + release * 10;
        while !quiet.all_idle.load(Ordering::Relaxed) {
            assert!(
                Instant::now() < deadline,
                "the quiet source was never released"
            );
            *lock(&busy.left) += 1;
            busy_reg.notify();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(quiet.done.load(Ordering::Relaxed), 4);
        assert!(quiet.idle.load(Ordering::Relaxed));
        assert!(quiet.per_worker.slots.iter().all(|s| lock(s).is_none()));
        assert!(
            !busy.all_idle.load(Ordering::Relaxed),
            "the busy source is not idle"
        );
    }

    #[test]
    fn idle_workers_free_their_state() {
        let release = Duration::from_millis(300);
        let pool = Pool::with_idle_release(2, "test-job", release);
        let counter = Arc::new(Counter {
            left: Mutex::new(10),
            done: AtomicUsize::new(0),
            per_worker: PerWorker::new(pool.threads()),
            idle: AtomicBool::new(false),
            all_idle: AtomicBool::new(false),
        });
        let reg = pool.register(counter.clone());
        reg.notify_all();
        let deadline = Instant::now() + release * 20;
        while !counter.all_idle.load(Ordering::Relaxed) {
            assert!(Instant::now() < deadline, "workers never released");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(counter.idle.load(Ordering::Relaxed));
        assert_eq!(counter.done.load(Ordering::Relaxed), 10);
        assert!(counter.per_worker.slots.iter().all(|s| lock(s).is_none()));
    }

    #[test]
    fn dropping_a_registration_waits_for_its_running_job() {
        let pool = Pool::new(2, "test-job");
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (tx, rx) = mpsc::channel();
        let held = Arc::new(Fixed {
            jobs: Mutex::new(
                [
                    (Rank::new(Class::Near, 0), "held"),
                    (Rank::new(Class::Near, 1), "second"),
                ]
                .into(),
            ),
            ran: tx,
            hold: Some(gate.clone()),
        });
        let reg = pool.register(held.clone());
        // A worker takes the job off the queue before holding it on the gate: wait for
        // that take, not a fixed sleep past it (a busy runner may start it later), so
        // `drop(reg)` below always has a running job to wait for.
        let picked = Instant::now();
        while lock(&held.jobs).len() > 1 {
            assert!(
                picked.elapsed() < Duration::from_secs(10),
                "no worker picked the job up"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let opener = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                *gate.0.lock().unwrap() = true;
                gate.1.notify_all();
            })
        };
        let began = Instant::now();
        drop(reg);
        // Both jobs may have started (two workers); none runs after the drop.
        assert!(began.elapsed() >= Duration::from_millis(150));
        opener.join().unwrap();
        let ran: Vec<_> = rx.try_iter().collect();
        assert!(ran.contains(&"held"));
        std::thread::sleep(Duration::from_millis(100));
        assert!(rx.try_iter().next().is_none());
        // The pool's own references are gone.
        assert_eq!(Arc::strong_count(&held), 1);
    }

    #[test]
    fn a_panicking_job_keeps_its_worker() {
        struct Boom(AtomicUsize);
        impl Source for Boom {
            fn next(&self) -> Option<Rank> {
                (self.0.load(Ordering::Relaxed) < 2).then_some(Rank::new(Class::Near, 0))
            }
            fn run(&self, _worker: usize) -> bool {
                if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                    panic!("first job fails");
                }
                true
            }
        }
        let pool = Pool::new(1, "test-job");
        let boom = Arc::new(Boom(AtomicUsize::new(0)));
        let reg = pool.register(boom.clone());
        reg.notify();
        let deadline = Instant::now() + Duration::from_secs(10);
        while boom.0.load(Ordering::Relaxed) < 2 {
            assert!(Instant::now() < deadline, "the worker died with the panic");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// What [`Inner`]'s counters cost on one cache line against a line each, the
/// pool's notify path and a worker's job loop on 8 threads: the two structures
/// are twins (same atomics, same operations, one generic body) so the only
/// difference measured is the layout (`cargo test --profile fast -p
/// horizon-threads --lib -- --ignored --nocapture padded`).
#[cfg(test)]
mod padding_bench {
    use super::Padded;
    use std::hint::black_box;
    use std::sync::atomic::{
        AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed, Ordering::SeqCst,
    };
    use std::time::{Duration, Instant};

    const WORKERS: usize = 8;
    /// The pool's counters, as [`super::Inner`] carries them.
    trait Counters: Sync {
        fn epoch(&self) -> &AtomicU64;
        fn sleepers(&self) -> &AtomicUsize;
        fn far(&self) -> &AtomicUsize;
        fn released(&self) -> &AtomicUsize;
        fn threads(&self) -> &AtomicUsize;
    }

    /// A line each: what the pool has now.
    struct Spaced {
        epoch: Padded<AtomicU64>,
        sleepers: Padded<AtomicUsize>,
        far: Padded<AtomicUsize>,
        released: Padded<AtomicUsize>,
        threads: Padded<AtomicUsize>,
    }

    impl Spaced {
        fn new() -> Self {
            Self {
                epoch: Padded::new(AtomicU64::new(0)),
                sleepers: Padded::new(AtomicUsize::new(0)),
                far: Padded::new(AtomicUsize::new(0)),
                released: Padded::new(AtomicUsize::new(0)),
                threads: Padded::new(AtomicUsize::new(WORKERS)),
            }
        }
    }

    /// All of them in one line: what the pool had before (aligned so that the
    /// five fit one line, the worst case of the layout being replaced).
    #[repr(C, align(64))]
    struct Packed {
        epoch: AtomicU64,
        sleepers: AtomicUsize,
        far: AtomicUsize,
        released: AtomicUsize,
        threads: AtomicUsize,
    }

    impl Packed {
        fn new() -> Self {
            Self {
                epoch: AtomicU64::new(0),
                sleepers: AtomicUsize::new(0),
                far: AtomicUsize::new(0),
                released: AtomicUsize::new(0),
                threads: AtomicUsize::new(WORKERS),
            }
        }
    }

    macro_rules! counters {
        ($($ty:ident),*) => {$(
            impl Counters for $ty {
                fn epoch(&self) -> &AtomicU64 {
                    &self.epoch
                }
                fn sleepers(&self) -> &AtomicUsize {
                    &self.sleepers
                }
                fn far(&self) -> &AtomicUsize {
                    &self.far
                }
                fn released(&self) -> &AtomicUsize {
                    &self.released
                }
                fn threads(&self) -> &AtomicUsize {
                    &self.threads
                }
            }
        )*};
    }
    counters!(Spaced, Packed);

    /// The frame or tick thread's notifies: one per job it queues.
    const NOTIFIES: u64 = 1_000_000;

    /// One job of a busy worker: `work`'s epoch, `choose`'s threads and far, a
    /// far place every eighth job (the compare-exchange that takes it and the
    /// fetch that ends it), a release of buffers every sixty-fourth. A busy
    /// worker never goes to sleep, so it never writes `sleepers`: that counter
    /// is the notifier's to read.
    fn job<C: Counters>(counters: &C, round: u64) {
        let _epoch = counters.epoch().load(SeqCst);
        let threads = counters.threads().load(Relaxed);
        if round % 8 == 0 {
            let mut far = counters.far().load(SeqCst);
            while far < threads.saturating_sub(1).max(1) {
                match counters
                    .far()
                    .compare_exchange_weak(far, far + 1, SeqCst, SeqCst)
                {
                    Ok(_) => break,
                    Err(now) => far = now,
                }
            }
            counters.far().fetch_sub(1, SeqCst);
        }
        if round % 64 == 0 {
            counters.released().fetch_add(1, Relaxed);
            counters.released().fetch_sub(1, Relaxed);
        }
    }

    /// One notify, as `Inner::notify` does it: bump the epoch, then look at the
    /// sleepers.
    fn notify<C: Counters>(counters: &C) {
        counters.epoch().fetch_add(1, SeqCst);
        black_box(counters.sleepers().load(SeqCst));
    }

    /// Seven busy workers and the one thread that queues their work: how long
    /// its notifies take (`notify` runs on this thread, timed), and what the
    /// workers got through in the same while.
    fn measure<C: Counters>(counters: &C) -> (Duration, u64) {
        let stop = AtomicBool::new(false);
        let stop = &stop;
        let done: Vec<AtomicU64> = (0..WORKERS - 1).map(|_| AtomicU64::new(0)).collect();
        let notified = std::thread::scope(|scope| {
            for slot in done.iter() {
                scope.spawn(move || {
                    let mut round = 0;
                    while !stop.load(Relaxed) {
                        job(counters, round);
                        round += 1;
                    }
                    slot.store(round, Relaxed);
                });
            }
            let start = Instant::now();
            for _ in 0..NOTIFIES {
                notify(counters);
            }
            let elapsed = start.elapsed();
            stop.store(true, Relaxed);
            elapsed
        });
        (notified, done.iter().map(|d| d.load(Relaxed)).sum())
    }

    /// The middle run's time and the median distance from it, in seconds.
    fn median_mad(mut times: Vec<f64>) -> (f64, f64) {
        times.sort_by(f64::total_cmp);
        let median = times[times.len() / 2];
        let mut deviations: Vec<f64> = times.iter().map(|t| (t - median).abs()).collect();
        deviations.sort_by(f64::total_cmp);
        (median, deviations[deviations.len() / 2])
    }

    /// Padded counters against packed ones, same access pattern on 8 threads:
    /// the padding has to make the notify path more than 5 % faster (the
    /// issue's gate) — the path the frame or tick thread takes to hand work to
    /// the pool, which is what a server's tick waits on. Each variant's median
    /// and MAD (the spread the machine adds) are printed, so a run on a noisy
    /// machine shows itself. Measurements only: run it by hand on an idle one.
    #[test]
    #[ignore = "performance measurement; run with --profile fast on an idle machine"]
    fn padded_counters_beat_packed_ones() {
        assert!(
            !std::hint::black_box(cfg!(debug_assertions)),
            "build with --profile fast"
        );
        let runs = 15;
        let mut spaced = Vec::with_capacity(runs);
        let mut packed = Vec::with_capacity(runs);
        let mut spaced_jobs = Vec::with_capacity(runs);
        let mut packed_jobs = Vec::with_capacity(runs);
        // One warm-up round of each, so neither pays for a cold heap or a cold
        // instruction cache.
        let (spaced_counters, packed_counters) = (Spaced::new(), Packed::new());
        measure(&spaced_counters);
        measure(&packed_counters);
        for run in 0..runs {
            // Interleaved and alternating which goes first, so a machine that
            // slows down or speeds up between runs hits both the same.
            for at in 0..2 {
                let spaced_now = (at == 0) == (run % 2 == 0);
                let (time, jobs) = if spaced_now {
                    measure(&spaced_counters)
                } else {
                    measure(&packed_counters)
                };
                if spaced_now {
                    spaced.push(time.as_secs_f64());
                    spaced_jobs.push(jobs as f64);
                } else {
                    packed.push(time.as_secs_f64());
                    packed_jobs.push(jobs as f64);
                }
            }
        }
        let (spaced_median, spaced_mad) = median_mad(spaced);
        let (packed_median, packed_mad) = median_mad(packed);
        let (spaced_ns, packed_ns) = (
            spaced_median / NOTIFIES as f64 * 1e9,
            packed_median / NOTIFIES as f64 * 1e9,
        );
        let gain = (packed_median / spaced_median - 1.0) * 100.0;
        println!(
            "notify on 7 busy workers, per notify: packed {packed_ns:.1} ns, MAD {:.1} %; padded {spaced_ns:.1} ns, MAD {:.1} %; padded is {gain:.1} % faster (gate 5 %: {})",
            packed_mad / packed_median * 100.0,
            spaced_mad / spaced_median * 100.0,
            if gain > 5.0 { "pass" } else { "fail" }
        );
        println!(
            "the 7 workers meanwhile: packed {:.2} M jobs, padded {:.2} M jobs",
            median_mad(packed_jobs).0 / 1e6,
            median_mad(spaced_jobs).0 / 1e6
        );
        assert!(
            spaced_median < packed_median,
            "padded state is not faster than packed"
        );
    }
}
