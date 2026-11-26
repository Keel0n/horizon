//! Which thread holds the memory, for measuring (#19332).
//!
//! The process memory map shows two kinds of mimalloc region: 1 GiB arenas, and regions of a
//! few megabytes and up that belong to a single allocation. In mimalloc v3 a request larger
//! than the largest size class (`MI_LARGE_MAX_OBJ_SIZE`, 64 KiB here) gets a *huge singleton
//! page* — one block per page, its own OS allocation, committed whole
//! (`page.c`: "Huge pages contain just one block, and the segment contains just that page").
//! A 32 MiB request is therefore a 32 MiB committed region that the map can see and cannot
//! name. This walk names it: every thread's own heap ("theap" in v3), the bytes it holds, and
//! each of its pages big enough to be one object — with how much of that page is live, which
//! is what tells a live-but-untouched buffer from a page the allocator kept after a free.
//!
//! Measuring only: nothing on a player's path calls [`report`], and the allocator that records
//! the threads is behind the `heap-owner` feature. In v3 all threads share one `mi_heap`; the
//! per-thread part is the theap, so the theap is what this walks and what names an owner.

use std::cell::Cell;
use std::sync::Mutex;

use libmimalloc_sys as mi;

const MB: usize = 1024 * 1024;

/// A page at least this big is listed on its own: it is one object of that size.
pub const BIG_AREA: usize = 8 * MB;

/// One page of one size class in one thread's heap.
#[derive(Clone)]
pub struct Area {
    /// Where the page's memory starts: the address a memory map shows for it.
    pub at: usize,
    /// Bytes reserved for this page.
    pub reserved: usize,
    /// Bytes of it committed now.
    pub committed: usize,
    /// Bytes of it holding live blocks.
    pub used: usize,
    /// The size of one block in it.
    pub block_size: usize,
    /// The thread whose heap holds it.
    pub owner: String,
}

/// One thread's heap: what it holds.
pub struct ThreadRow {
    /// The thread that made it, or `unnamed`.
    pub thread: String,
    /// Its theap's address, which is what makes it distinct.
    pub theap: usize,
    /// Bytes reserved in it.
    pub reserved: usize,
    /// Bytes committed in it.
    pub committed: usize,
    /// Bytes of live blocks in it.
    pub used: usize,
    /// How many pages it holds.
    pub pages: usize,
}

/// Every thread's heap of this process, and the big pages among them.
pub struct Report {
    /// The threads, largest committed first.
    pub threads: Vec<ThreadRow>,
    /// Pages of at least [`BIG_AREA`], largest first: one object each.
    pub big: Vec<Area>,
}

/// The theap each recording thread made, by address.
static OWNERS: Mutex<Vec<(usize, &'static str)>> = Mutex::new(Vec::new());

thread_local! {
    /// Set when the thread has recorded its heap: once per thread is enough.
    static RECORDED: Cell<bool> = const { Cell::new(false) };
}

/// mimalloc's `mi_theap_t`, opaque here.
#[repr(C)]
struct Theap {
    _private: [u8; 0],
}

unsafe extern "C" {
    /// The calling thread's own heap, created on first use.
    fn mi_theap_get_default() -> *mut Theap;
    /// Visit every page of a theap as an area. Deprecated in mimalloc's own header, and still
    /// the only way to see another thread's pages.
    fn mi_theap_visit_blocks(
        theap: *const Theap,
        visit_blocks: bool,
        visitor: mi::mi_block_visit_fun,
        arg: *mut std::ffi::c_void,
    ) -> bool;
}

/// Record the calling thread's heap and name, on its first allocation.
///
/// Only [`OwnerAllocator`] calls this. Reading the thread's name allocates, which lands back
/// here; `RECORDED` is set first so that call answers immediately instead of recursing.
pub fn record_heap() {
    if RECORDED.replace(true) {
        return;
    }
    // SAFETY: reads the calling thread's own heap, creating it if the thread has none. Called
    // from inside the allocator, so it must not allocate before `RECORDED` is set — it does not.
    let theap = unsafe { mi_theap_get_default() } as usize;
    if theap == 0 {
        return;
    }
    let name = std::thread::current()
        .name()
        .map(str::to_owned)
        .unwrap_or_default();
    let name: &'static str = if name.is_empty() {
        "unnamed"
    } else {
        Box::leak(name.into_boxed_str())
    };
    if let Ok(mut owners) = OWNERS.lock() {
        if !owners.iter().any(|(recorded, _)| *recorded == theap) {
            owners.push((theap, name));
        }
    }
}

/// What one theap's walk collects: its totals, and the big pages in it.
#[derive(Default)]
struct TheapWalk {
    row: Option<ThreadRow>,
    big: Vec<Area>,
}

unsafe extern "C" fn visit_area(
    _heap: *const mi::mi_heap_t,
    area: *const mi::mi_heap_area_t,
    block: *mut std::ffi::c_void,
    _block_size: usize,
    arg: *mut std::ffi::c_void,
) -> bool {
    // The walk calls this once per page (`block` null), and then — only when asked to visit
    // blocks, which the report does not — once per live block.
    if block.is_null() && !area.is_null() {
        // SAFETY: `area` is a live `mi_heap_area_t` for this call, and the report passes the
        // `TheapWalk` it is filling in `arg`.
        let (area, walk) = unsafe { (&*area, &mut *arg.cast::<TheapWalk>()) };
        if let Some(row) = walk.row.as_mut() {
            row.reserved = row.reserved.saturating_add(area.reserved);
            row.committed = row.committed.saturating_add(area.committed);
            row.used = row.used.saturating_add(area.used);
            row.pages += 1;
        }
        if area.reserved >= BIG_AREA {
            walk.big.push(Area {
                at: area.blocks as usize,
                reserved: area.reserved,
                committed: area.committed,
                used: area.used,
                block_size: area.block_size,
                owner: walk
                    .row
                    .as_ref()
                    .map(|row| row.thread.clone())
                    .unwrap_or_default(),
            });
        }
    }
    true
}

/// Every thread's heap of this process, and the big pages among them.
///
/// A thread being allocated from while this runs is read the way mimalloc's own statistics
/// print reads it (`mi_subproc_heap_stats_print_out` walks the same lists): torn at worst,
/// never unsafe. The walk does not lock a theap, so run it when the work is quiet.
pub fn report() -> Report {
    let owners = OWNERS.lock().map(|o| o.clone()).unwrap_or_default();
    let mut threads = Vec::with_capacity(owners.len());
    let mut big = Vec::new();
    for (theap, name) in &owners {
        if *theap == 0 {
            continue;
        }
        let mut walk = TheapWalk {
            row: Some(ThreadRow {
                thread: (*name).to_owned(),
                theap: *theap,
                reserved: 0,
                committed: 0,
                used: 0,
                pages: 0,
            }),
            big: Vec::new(),
        };
        // SAFETY: walks a theap of this process. The visitor only reads the area it is given
        // and adds up numbers; nothing is allocated and nothing is freed.
        unsafe {
            mi_theap_visit_blocks(
                (*theap) as *const Theap,
                false,
                Some(visit_area),
                (&raw mut walk).cast(),
            );
        }
        big.append(&mut walk.big);
        if let Some(row) = walk.row {
            threads.push(row);
        }
    }
    threads.sort_by_key(|row| std::cmp::Reverse(row.committed));
    big.sort_by_key(|area| std::cmp::Reverse(area.committed));
    Report { threads, big }
}

#[cfg(feature = "heap-owner")]
pub use recording::OwnerAllocator;

#[cfg(feature = "heap-owner")]
mod recording {
    use std::alloc::{GlobalAlloc, Layout};

    /// mimalloc with every thread's heap recorded ([`super::record_heap`]), for measuring.
    pub struct OwnerAllocator;

    // SAFETY: every call is forwarded to mimalloc unchanged; the recording only reads the
    // calling thread's own heap and name, on the thread's first allocation.
    unsafe impl GlobalAlloc for OwnerAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            super::record_heap();
            // SAFETY: `layout` came from a caller of this allocator with a nonzero size.
            unsafe { mimalloc::MiMalloc.alloc(layout) }
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            // SAFETY: `block` came from this allocator with `layout`.
            unsafe { mimalloc::MiMalloc.dealloc(block, layout) }
        }

        unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: `block` came from this allocator with `layout`.
            unsafe { mimalloc::MiMalloc.realloc(block, layout, new_size) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BIG_AREA, report};

    #[test]
    fn the_report_lists_the_calling_threads_heap() {
        // Allocating first makes sure this thread has a heap to find.
        let block = vec![0u8; 64];
        let report = report();
        assert!(!report.threads.is_empty(), "the process has at least one heap");
        assert!(
            report.threads.iter().any(|row| row.committed > 0),
            "a heap that just allocated has committed bytes"
        );
        std::hint::black_box(&block);
    }

    #[test]
    fn a_big_page_appears_with_its_size_and_its_owner() {
        let block = vec![7u8; 16 * 1024 * 1024];
        let report = report();
        let area = report
            .big
            .iter()
            .find(|area| area.used >= block.len())
            .expect("a 16 MiB block has a page of its own");
        assert!(area.reserved >= BIG_AREA);
        assert!(
            report.threads.iter().any(|row| row.theap > 0),
            "the page's owner is one of the heaps listed"
        );
        std::hint::black_box(&block);
    }
}
