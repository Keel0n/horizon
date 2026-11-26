//! The heap the program has in use in mimalloc, from its statistics.
//!
//! Two kinds of number come out of mimalloc here, and they are not equally trustworthy. The
//! arena totals (`reserved`, `committed`, `purged`, `arena_count`) are kept by the subprocess
//! itself and match a memory map of the process. The per-size-class page counts are kept per
//! thread heap and merged into the subprocess on each collect; a count read between merges is a
//! little stale and, where a heap's own figure is added twice, larger than the truth — which is
//! why the bytes they imply (`requested` in mimalloc's own printout) can read several times the
//! process's commit. Only the first kind is reported as a measurement.

/// The number after `"key":` on a line of mimalloc's JSON.
fn field(line: &str, key: &str) -> Option<usize> {
    let from = line.find(&format!("\"{key}\":"))? + key.len() + 3;
    digits(&line[from..])
}

/// Bytes of the pages mimalloc holds now: `page_bins` counts the pages of each size class (pages,
/// not blocks) and `page_size` is what a page of that class costs. The huge class prints no
/// `page_size`: its blocks have no size class of their own, and the statistics say only how many
/// huge pages there are, not how big, so they are left out.
///
/// Stale between merges (see the module note): it answers "roughly how much of the heap is in
/// pages", not "how many bytes does the inbox hold".
fn in_use(json: &str) -> Option<usize> {
    let bins = json.split("\"page_bins\"").nth(1)?;
    let bins = bins.split(']').next()?;
    let mut total = 0;
    for line in bins.lines().filter(|l| l.contains("\"block_size\"")) {
        let page_size = field(line, "page_size")?;
        if page_size == 0 {
            continue;
        }
        total += field(line, "current")? * page_size;
    }
    Some(total)
}

fn stats_json() -> Option<String> {
    // SAFETY: a null buffer asks mimalloc for one of its own, freed below with its `mi_free`.
    let raw = unsafe { libmimalloc_sys::mi_stats_get_json(0, std::ptr::null_mut()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: mimalloc returns a NUL-terminated string; `.cast()` bridges `cty::c_char` (`u8` on Windows ARM64) to `std::ffi::c_char`.
    let text = unsafe { std::ffi::CStr::from_ptr(raw.cast()) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: `raw` came from `mi_stats_get_json` with no buffer given.
    unsafe { libmimalloc_sys::mi_free(raw.cast()) };
    Some(text)
}

/// The unsigned number that follows `text`, ignoring leading spaces. mimalloc prints its counters
/// as signed numbers and adjusts some of them down while others read them (reserving an arena
/// subtracts its size before committing it), so a read can catch a negative: it counts as nothing.
fn digits(text: &str) -> Option<usize> {
    let text = text.trim_start();
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let number: String = text.chars().take_while(char::is_ascii_digit).collect();
    let value: usize = number.parse().ok()?;
    Some(if negative { 0 } else { value })
}

/// The `current` of a counted field, which mimalloc prints as
/// `"pages": { "total": …, "peak": …, "current": … }`.
fn counted(json: &str, key: &str) -> Option<usize> {
    let object = &json[json.find(&format!("\"{key}\": {{"))?..];
    let at = object.find("\"current\":")? + "\"current\":".len();
    digits(&object[at..])
}

/// The value of a counter, which mimalloc prints as `"purged": 123,`.
fn counter(json: &str, key: &str) -> Option<usize> {
    let at = json.find(&format!("\"{key}\":"))? + key.len() + 3;
    digits(&json[at..])
}

/// The arenas' address space, how much of it is committed, and how much mimalloc has given back.
///
/// `committed` is the number that matters for #19332: a render distance cycle leaves the arenas'
/// commit higher than it found it, and this figure says by how much without a memory map of the
/// process. `purged` is cumulative, so a run's own purging is the difference between two reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Arenas {
    /// Address space the arenas have reserved.
    pub reserved: usize,
    /// Bytes they have committed.
    pub committed: usize,
    /// Bytes given back to the OS since the start.
    pub purged: usize,
    pub count: usize,
}

/// The arenas' numbers now: what a cycle's leftover shows up in.
pub fn arenas() -> Option<Arenas> {
    let json = stats_json()?;
    Some(Arenas {
        reserved: counted(&json, "reserved")?,
        committed: counted(&json, "committed")?,
        purged: counter(&json, "purged")?,
        count: counter(&json, "arena_count")?,
    })
}

/// The bytes of the pages holding allocations now: the heap in use, counting each page whole.
/// A huge block (one bigger than the largest size class) is not counted: this build's statistics do
/// not say how big one is, so the live figure leaves them out and `overhead_mb` takes them in.
pub fn live_bytes() -> Option<usize> {
    in_use(&stats_json()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_arena_totals_apart_from_the_pages_they_hold() {
        // The shape mimalloc prints: counted fields as an object with `current`, counters as a
        // plain number, then the bins, whose `page_size` is not a total.
        let json = r#"{
  "process": { "elapsed_msecs": 1000, "rss_current": 700 },
  "pages": { "total": 9000, "peak": 12000, "current": 9004 },
  "reserved": { "total": 0, "peak": 2147483648, "current": 2147483648 },
  "committed": { "total": 0, "peak": 3000000000, "current": 2952790016 },
  "page_committed": { "total": 0, "peak": 2800000000, "current": 2415919104 },
  "pages_abandoned": { "total": 0, "peak": 40, "current": 17 },
  "theaps": { "total": 0, "peak": 31, "current": 22 },
  "purged": 536870912,
  "arena_count": 9,
  "page_bins": [
    { "total": 5, "peak": 5, "current": 3, "block_size": 112, "page_size": 65536 },
    { "total": 4, "peak": 4, "current": 2, "block_size": 16384, "page_size": 524288 },
    { "total": 3, "peak": 3, "current": 3, "block_size": 524296, "page_size": 0 }
  ]
}"#;
        let arenas = Arenas {
            reserved: 2147483648,
            committed: 2952790016,
            purged: 536870912,
            count: 9,
        };
        assert_eq!(counted(json, "reserved"), Some(arenas.reserved));
        assert_eq!(counted(json, "committed"), Some(arenas.committed));
        assert_eq!(counter(json, "purged"), Some(arenas.purged));
        assert_eq!(counter(json, "arena_count"), Some(arenas.count));
        // The page bytes come from the bins: a page of each class, not a total of any, and the
        // huge class (no page size) is left out.
        assert_eq!(in_use(json), Some(3 * 65536 + 2 * 524288));
        // A key the build does not print is not guessed at, and a peak is not a current.
        assert_eq!(counted(json, "segments"), None);
        assert_eq!(counter(json, "reset"), None);
        assert_eq!(counted(json, "theaps"), Some(22));
        // A counter read while the allocator adjusts it down can come out negative: it counts as
        // nothing rather than losing the whole figure.
        assert_eq!(digits(" -4096,"), Some(0));
        assert_eq!(digits(" 4096,"), Some(4096));
        assert_eq!(digits(" nothing,"), None);
    }

    #[test]
    fn counts_a_page_once_however_many_blocks_it_holds() {
        let json = r#"{
  "malloc_huge": { "total": 9, "peak": 9, "current": 1000 },
  "malloc_bins": [
    { "total": 9, "peak": 9, "current": 3, "block_size": 112, "page_size": 65536 },
    { "total": 1, "peak": 1, "current": 2, "block_size": 524296, "page_size": 0 }
  ],
  "page_bins": [
    { "total": 5, "peak": 5, "current": 1, "block_size": 112, "page_size": 65536 },
    { "total": 4, "peak": 4, "current": 2, "block_size": 16384, "page_size": 524288 },
    { "total": 3, "peak": 3, "current": 3, "block_size": 524296, "page_size": 0 }
  ]
}"#;
        // Three blocks live in the 112-byte bin's one page; the three huge pages have no page size.
        assert_eq!(in_use(json), Some(65536 + 2 * 524288));
    }

    #[test]
    fn a_freed_large_allocation_returns_to_its_baseline() {
        // The first call makes the buffer the statistics are printed into, and its pages count
        // from the next call on: read once so the baseline holds them.
        live_bytes().unwrap();
        let before = live_bytes().unwrap();
        // SAFETY: allocates and frees one block with mimalloc.
        let block = unsafe { libmimalloc_sys::mi_malloc(64 << 20) };
        let during = live_bytes().unwrap();
        let (commit, _) = crate::allocator_commit();
        // SAFETY: `block` came from `mi_malloc` above.
        unsafe { libmimalloc_sys::mi_free(block) };
        let after = live_bytes().unwrap();
        // A huge block has no page size to count, and leaving it out is what keeps the figure
        // under `commit_mb`; a cumulative counter would have kept the 64 MiB in `after`. The
        // statistics buffer itself may take or release a page between the reads, so allow a
        // few pages of slack: the 64 MiB block would be far above it.
        let slack = 4 << 20;
        assert!(during <= commit, "{during} over {commit} committed");
        assert!(
            after <= before + slack,
            "{before} then {during} then {after}"
        );
    }

    /// The arenas' own numbers are readable in a running program, and committing allocates more
    /// than the process had before.
    #[test]
    fn arena_commit_grows_with_a_large_allocation() {
        let _ = arenas().unwrap();
        let before = arenas().unwrap();
        // SAFETY: allocates and frees one block with mimalloc.
        let block = unsafe { libmimalloc_sys::mi_malloc(8 << 20) };
        assert!(!block.is_null());
        let during = arenas().unwrap();
        // SAFETY: `block` came from `mi_malloc` above.
        unsafe { libmimalloc_sys::mi_free(block) };
        assert!(
            during.committed >= before.committed,
            "{} then {}",
            before.committed,
            during.committed
        );
        assert!(during.reserved > 0 && during.count > 0);
    }
}
