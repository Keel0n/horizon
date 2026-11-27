//! Which thread holds the memory, for measuring (#19332).
//!
//! The process memory map shows two kinds of mimalloc region: 1 GiB arenas, and regions of a
//! few megabytes and up that belong to a single allocation. In mimalloc v3 a request larger
//! than the largest size class (`MI_LARGE_MAX_OBJ_SIZE`, 64 KiB here) gets a *huge singleton
//! page* вЂ” one block per page, its own OS allocation, committed whole
//! (`page.c`: "Huge pages contain just one block, and the segment contains just that page").
//! A 32 MiB request is therefore a 32 MiB committed region that the map can see and cannot
//! name. The heap-level walk names it: v3 tracks singleton pages on the heap, not on the
//! thread's queues (they are "abandoned" at once, `page.c:mi_huge_page_alloc`), so
//! `mi_heap_visit_blocks` over the default heap вЂ” the locked, supported walk, the one
//! mimalloc's own statistics print uses вЂ” sees every one of them, and each area carries the
//! page pointer (`reserved1`), whose first field is the owning thread's id
//! (`types.h:mi_page_t.xthread_id`, with the page flags in its bottom three bits).
//!
//! A theap's page queues may not be read from another thread (`mi_theap_visit_blocks` is
//! deprecated for that reason, and a walk from the control thread is what the first build of
//! this module crashed on); the heap walk takes locks and is the walk mimalloc itself does.
//!
//! Measuring only: nothing on a player's path depends on the numbers, and the allocator that
//! records the threads is behind the `heap-owner` feature.

use std::cell::Cell;
use std::sync::Mutex;

use libmimalloc_sys as mi;

const MB: usize = 1024 * 1024;

/// A page at least this big is listed on its own: it is one object of that size.
pub const BIG_AREA: usize = 8 * MB;

/// One page of one size class in the heap.
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
    /// The thread that owns it, or `abandoned` where mimalloc owns it itself.
    pub owner: String,
}

/// A thread that recorded itself: its name and the OS id its pages carry.
pub struct ThreadRow {
    /// The thread's name, or `unnamed`.
    pub thread: String,
    /// The OS thread id, as the pages name their owner.
    pub id: usize,
}

/// One thread's pages, added up: how much of the allocator's memory that thread's heap holds.
/// `committed` minus `used` is what the thread's own collect could give back; `free_pages` pages
/// have no live block at all, so a collect releases them whole.
pub struct OwnerRow {
    /// The thread that owns the pages, or `abandoned` where no thread's queue holds them.
    pub thread: String,
    /// How many pages the thread's heap holds.
    pub pages: usize,
    /// Bytes reserved for them.
    pub reserved: usize,
    /// Bytes of them committed now; this is the sum the memory map's arenas show.
    pub committed: usize,
    /// Bytes of them holding live blocks.
    pub used: usize,
    /// Pages with no live block: a collect returns each of them.
    pub free_pages: usize,
    /// Bytes committed in pages that hold no live block.
    pub free_committed: usize,
    /// Bytes committed in pages that do hold live blocks but have room left: collect keeps them.
    pub partly_committed: usize,
}

/// What the heap walk saw: the threads that recorded themselves, and the big pages.
pub struct Report {
    /// The threads that recorded themselves, in the order they recorded.
    pub threads: Vec<ThreadRow>,
    /// Pages of at least [`BIG_AREA`] the live walk found, largest first: one object each.
    pub big: Vec<Area>,
    /// The same for pages no thread's queue holds (abandoned ones, and singletons, which v3
    /// tracks on the heap): where a freed-but-not-yet-released big page shows up.
    pub abandoned: Vec<Area>,
    /// Every page the walk saw, added up per owning thread, most committed first: the answer to
    /// "whose heap holds the memory" (#19332).
    pub owners: Vec<OwnerRow>,
}

/// The first field of mimalloc's `mi_page_t`: the owning thread's id, with the page flags in
/// its bottom three bits (`types.h:382-383`). Read through the area's `reserved1`.
#[repr(C)]
struct PageHead {
    xthread_id: usize,
}

/// The page flags in the bottom bits of `xthread_id` (`types.h:337`: `MI_PAGE_FLAG_MASK` is
/// `0x03`, two bits, and an abandoned page carries the marker `0x04` in them).
const PAGE_FLAG_MASK: usize = 0x03;
/// The `xthread_id` of a page no thread's queue holds (`types.h:345`,
/// `MI_THREADID_ABANDONED_MAPPED`), and of a page with no owner at all.
const THREADID_ABANDONED: usize = 0x04;

/// What the heap walk collects.
#[derive(Default)]
struct HeapWalk {
    big: Vec<Area>,
    /// Per owning thread: pages, reserved, committed, used, free pages, bytes in free pages.
    owners: Vec<(usize, usize, usize, usize, usize, usize, usize)>,
}

/// Add one page to its owner's row, creating the row the first time. Linear over a handful of
/// threads, and it allocates no string: the visitor runs under mimalloc's locks.
fn add_to_owner(
    owners: &mut Vec<(usize, usize, usize, usize, usize, usize, usize)>,
    id: usize,
    reserved: usize,
    committed: usize,
    used: usize,
) {
    let free = used == 0;
    if let Some(row) = owners.iter_mut().find(|row| row.0 == id) {
        row.1 += 1;
        row.2 += reserved;
        row.3 += committed;
        row.4 += used;
        if free {
            row.5 += 1;
            row.6 += committed;
        }
    } else {
        owners.push((
            id,
            1,
            reserved,
            committed,
            used,
            usize::from(free),
            if free { committed } else { 0 },
        ));
    }
}

unsafe extern "C" fn visit_area(
    _heap: *const mi::mi_heap_t,
    area: *const mi::mi_heap_area_t,
    block: *mut std::ffi::c_void,
    _block_size: usize,
    arg: *mut std::ffi::c_void,
) -> bool {
    // The walk calls this once per page (`block` null), and then вЂ” only when asked to visit
    // blocks, which this does not вЂ” once per live block.
    if block.is_null() && !area.is_null() {
        // SAFETY: `area` is a live `mi_heap_area_t` for this call, and the walk passes the
        // `HeapWalk` it is filling in `arg`.
        let (area, walk) = unsafe { (&*area, &mut *arg.cast::<HeapWalk>()) };
        // SAFETY: `reserved1` is the `mi_page_t` of the area being visited; the walk holds
        // the heap's locks, so the page metadata is stable, and only the first field is read.
        let id = unsafe {
            area.reserved1
                .cast::<PageHead>()
                .as_ref()
                .map_or(0, |page| page.xthread_id & !PAGE_FLAG_MASK)
        };
        add_to_owner(
            &mut walk.owners,
            id,
            area.reserved,
            area.committed,
            area.used.saturating_mul(area.block_size),
        );
        if area.reserved >= BIG_AREA {
            walk.big.push(Area {
                at: area.blocks as usize,
                reserved: area.reserved,
                committed: area.committed,
                used: area.used,
                block_size: area.block_size,
                owner: name_of(id),
            });
        }
    }
    true
}

/// What each thread that recorded itself published: `(name, os thread id)`.
static NAMED: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());

/// mimalloc's `mi_heap_t`, opaque here.
#[repr(C)]
struct Heap {
    _private: [u8; 0],
}

unsafe extern "C" {
    /// The main heap, which owns the process's pages. (v3's `mi_heap_get_backing` is gone;
    /// `mi_heap_main` is its successor in `mimalloc.h:239`.)
    fn mi_heap_main() -> *mut Heap;
    /// Visit all areas and blocks in a heap, under the heap's locks.
    fn mi_heap_visit_blocks(
        heap: *mut Heap,
        visit_all_blocks: bool,
        visitor: mi::mi_block_visit_fun,
        arg: *mut std::ffi::c_void,
    ) -> bool;
    /// Visit only the pages the heap holds but no thread's queue does: pages whose thread gave
    /// them up, and singleton pages, which v3 tracks on the heap (they are abandoned at once,
    /// `page.c:mi_huge_page_alloc`).
    fn mi_heap_visit_abandoned_blocks(
        heap: *mut Heap,
        visit_all_blocks: bool,
        visitor: mi::mi_block_visit_fun,
        arg: *mut std::ffi::c_void,
    ) -> bool;
}

/// The id mimalloc writes into a page's `xthread_id`: `_mi_prim_thread_id()`
/// (`include/mimalloc/prim.h:292-297`), which on Windows is **the thread's TEB address**, not
/// `GetCurrentThreadId`. Reading the wrong one made every owner print as `thread 0x…`: the
/// first build of this module used the Win32 thread id and no name ever matched.
#[cfg(all(windows, target_arch = "x86_64"))]
fn own_thread_id() -> usize {
    let teb: usize;
    // SAFETY: reads the TEB pointer the kernel keeps in the GS segment's first slot; no memory
    // of this process is written, and the read does not allocate.
    unsafe {
        core::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, readonly, preserves_flags));
    }
    teb
}

/// The calling thread's id as mimalloc writes it into its pages: not read here.
#[cfg(not(all(windows, target_arch = "x86_64")))]
fn own_thread_id() -> usize {
    0
}

/// A thread's name by its OS id, or the id itself where nothing recorded it. `0` and
/// [`THREADID_ABANDONED`] are what mimalloc writes on a page its thread gave up (abandoned).
fn name_of(id: usize) -> String {
    if id == 0 || id == THREADID_ABANDONED {
        return "abandoned".to_string();
    }
    NAMED
        .lock()
        .ok()
        .and_then(|named| {
            named
                .iter()
                .find(|(_, nid)| *nid == id)
                .map(|(name, _)| name.clone())
        })
        .unwrap_or_else(|| format!("thread {id:#x}"))
}

/// Record the calling thread's name and OS id, from its 64th allocation on. The mapping is
/// what turns the heap walk's owner ids into thread names.
pub fn record_heap() {
    thread_local! {
        /// Allocations this thread made; the name waits out the thread's own setup.
        static ALLOCS: Cell<u32> = const { Cell::new(0) };
        /// Set once the thread's name is recorded: once per thread is enough.
        static NAMED_HERE: Cell<bool> = const { Cell::new(false) };
    }
    if own_thread_id() == 0 || ALLOCS.replace(64) >= 64 || NAMED_HERE.replace(true) {
        return;
    }
    let name = std::thread::current()
        .name()
        .map(str::to_owned)
        .unwrap_or_else(|| "unnamed".to_string());
    if let Ok(mut named) = NAMED.lock() {
        if !named.iter().any(|(_, id)| *id == own_thread_id()) {
            named.push((name, own_thread_id()));
        }
    }
}

/// Publish the calling thread's name and OS id from outside the allocator (a control command,
/// a between-jobs worker); the allocator path is [`record_heap`].
pub fn publish_self() {
    record_heap();
}

/// Walk the heap of this process and report the big pages and the threads that recorded
/// themselves. The heap walk is the one mimalloc's own statistics print does.
pub fn report() -> Report {
    let mut walk = HeapWalk::default();
    let mut abandoned = HeapWalk::default();
    // SAFETY: walks this process's default heap under mimalloc's locks; the visitors only
    // read the areas they are given. The calling thread holds no other mimalloc locks.
    unsafe {
        let heap = mi_heap_main();
        mi_heap_visit_blocks(heap, false, Some(visit_area), (&raw mut walk).cast());
        mi_heap_visit_abandoned_blocks(heap, false, Some(visit_area), (&raw mut abandoned).cast());
    }
    walk.big
        .sort_by_key(|area| std::cmp::Reverse(area.committed));
    abandoned
        .big
        .sort_by_key(|area| std::cmp::Reverse(area.committed));
    // Both walks, added up per owning thread: the live pages and the ones no thread's queue
    // holds are different pages, so their sums are the whole heap.
    let mut totals = walk.owners;
    for row in abandoned.owners {
        match totals.iter_mut().find(|total| total.0 == row.0) {
            Some(total) => {
                total.1 += row.1;
                total.2 += row.2;
                total.3 += row.3;
                total.4 += row.4;
                total.5 += row.5;
                total.6 += row.6;
            }
            None => totals.push(row),
        }
    }
    let mut owners: Vec<OwnerRow> = totals
        .into_iter()
        .map(
            |(id, pages, reserved, committed, used, free_pages, free_committed)| OwnerRow {
                thread: name_of(id),
                pages,
                reserved,
                committed,
                used,
                free_pages,
                free_committed,
                partly_committed: committed.saturating_sub(free_committed),
            },
        )
        .collect();
    owners.sort_by_key(|row| std::cmp::Reverse(row.committed));
    let threads = NAMED
        .lock()
        .map(|named| {
            named
                .iter()
                .map(|(name, id)| ThreadRow {
                    thread: name.clone(),
                    id: *id,
                })
                .collect()
        })
        .unwrap_or_default();
    Report {
        threads,
        big: walk.big,
        abandoned: abandoned.big,
        owners,
    }
}

#[cfg(feature = "heap-owner")]
pub use recording::OwnerAllocator;

#[cfg(feature = "heap-owner")]
mod recording {
    use std::alloc::{GlobalAlloc, Layout};

    /// mimalloc that has every thread record its name ([`super::record_heap`]), for measuring.
    pub struct OwnerAllocator;

    // SAFETY: every call is forwarded to mimalloc unchanged; the recording publishes only the
    // calling thread's name and id, from its 64th allocation on.
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
    use super::{record_heap, report};

    #[test]
    fn the_heap_walk_sees_the_big_page() {
        record_heap();
        // The test binary's `vec!` goes to the system allocator; a raw mimalloc block is what
        // the walk can see.
        let block = unsafe { libmimalloc_sys::mi_malloc(16 * 1024 * 1024) };
        assert!(!block.is_null());
        unsafe { std::ptr::write_bytes(block.cast::<u8>(), 7, 16 * 1024 * 1024) };
        let report = report();
        assert!(
            report
                .big
                .iter()
                .any(|area| area.used >= 1 && area.committed >= 16 * 1024 * 1024),
            "a 16 MiB block has a page of its own in the heap walk"
        );
        // SAFETY: `block` came from `mi_malloc` above, with its full size.
        unsafe { libmimalloc_sys::mi_free(block) };
    }

    #[test]
    fn only_big_pages_are_listed() {
        record_heap();
        let small = vec![1u8; 64];
        let report = report();
        assert!(
            report
                .big
                .iter()
                .all(|area| area.reserved >= super::BIG_AREA),
            "only pages of the big size are listed"
        );
        std::hint::black_box(&small);
    }
}
