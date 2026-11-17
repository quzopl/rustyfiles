//! Duplicate files under a folder (the "Find duplicates" view).
//!
//! In steps, each reading only what the one before could not tell apart:
//! 1. the folder is walked by [`DiskTree::build`] (parallel, below-normal priority, reparse points neither counted nor followed);
//! 2. files are grouped by size: a size that occurs once can have no duplicate, and empty files are left out;
//! 3. files of a shared size are told apart by a SHA-256 of their first 64 KiB (for a smaller file that is the whole file);
//! 4. what is still alike is hashed whole with SHA-256.
//!
//! Hard links of one file (the same volume serial and file index) are one file, not duplicates: deleting one frees nothing.
//! Blocking: [`DupeScan`] runs it on a thread of its own.

use std::collections::HashMap;
use std::fs::File;
use std::hash::Hash;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::SystemTime;

use parking_lot::Mutex;
use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::diskusage::{self, DiskTree};

/// What is read of every candidate first.
const HEAD: u64 = 64 * 1024;
const BUF: usize = 1 << 20;

/// One copy of a duplicated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupeFile {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
}

/// Files with the same contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupeGroup {
    /// The size of each copy.
    pub size: u64,
    /// Two or more, oldest first (the first is the one kept by "select all but the original").
    pub files: Vec<DupeFile>,
}

impl DupeGroup {
    /// The space taken by the copies beyond one.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64).saturating_sub(1)
    }
}

/// What a finished (or cancelled) scan found.
#[derive(Debug, Default)]
pub struct DupeResult {
    /// Biggest waste first.
    pub groups: Vec<DupeGroup>,
    /// Files looked at (all of them, before grouping).
    pub files: u64,
    /// Files that could not be read while comparing (locked, access denied), left out.
    pub unreadable: u64,
    /// Folders the walk could not list.
    pub skipped_dirs: u32,
    pub cancelled: bool,
}

impl DupeResult {
    pub fn wasted(&self) -> u64 {
        self.groups.iter().map(DupeGroup::wasted).sum()
    }
}

/// Which step a scan is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Walking the folder: `entries` counts what was found.
    Walking,
    /// Reading candidates: `hashed` of `to_hash` files, `bytes` read.
    Comparing,
    Done,
}

/// Readable from any thread while the scan runs.
#[derive(Default)]
pub struct Progress {
    phase: AtomicU8,
    walk: diskusage::Progress,
    pub to_hash: AtomicU64,
    pub hashed: AtomicU64,
    pub bytes: AtomicU64,
}

impl Progress {
    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Relaxed) {
            0 => Phase::Walking,
            1 => Phase::Comparing,
            _ => Phase::Done,
        }
    }

    /// Entries found by the walk so far.
    pub fn entries(&self) -> u64 {
        self.walk.entries.load(Ordering::Relaxed)
    }
}

/// Groups `items` by `key` and keeps the groups of two or more.
pub fn shared<T, K: Hash + Eq>(items: Vec<T>, key: impl Fn(&T) -> K) -> Vec<Vec<T>> {
    let mut m: HashMap<K, Vec<T>> = HashMap::new();
    for it in items {
        m.entry(key(&it)).or_default().push(it);
    }
    m.into_values().filter(|g| g.len() > 1).collect()
}

/// A file as the comparison reads it.
struct Candidate {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    /// (volume serial, file index): the same for every hard link of one file.
    id: Option<(u32, u64)>,
    head: [u8; 32],
}

fn file_id(f: &File) -> Option<(u32, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle belongs to `f`, which is open for the whole call; `info` is a valid out-pointer.
    unsafe { GetFileInformationByHandle(HANDLE(f.as_raw_handle()), &mut info) }.ok()?;
    Some((info.dwVolumeSerialNumber, (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)))
}

/// SHA-256 of up to `limit` bytes of `r`, adding what was read to `bytes`. `None` when cancelled or on a read error.
fn sha_of(r: &mut impl Read, limit: u64, cancel: &AtomicBool, bytes: &AtomicU64) -> Option<[u8; 32]> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; BUF.min(limit.max(1) as usize)];
    let mut left = limit;
    while left > 0 {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let want = buf.len().min(left as usize);
        let n = r.read(&mut buf[..want]).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        bytes.fetch_add(n as u64, Ordering::Relaxed);
        left -= n as u64;
    }
    Some(h.finalize().into())
}

fn read_head(path: PathBuf, size: u64, cancel: &AtomicBool, bytes: &AtomicU64) -> Option<Candidate> {
    let mut f = File::open(&path).ok()?;
    let id = file_id(&f);
    let modified = f.metadata().ok().and_then(|m| m.modified().ok());
    let head = sha_of(&mut f, size.min(HEAD), cancel, bytes)?;
    Some(Candidate { path, size, modified, id, head })
}

fn read_whole(c: &Candidate, cancel: &AtomicBool, bytes: &AtomicU64) -> Option<[u8; 32]> {
    let mut f = File::open(&c.path).ok()?;
    sha_of(&mut f, c.size, cancel, bytes)
}

/// One file per hard-linked set: the first path of each file id stays.
fn one_per_file(mut g: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen = std::collections::HashSet::new();
    g.retain(|c| c.id.is_none_or(|id| seen.insert(id)));
    g
}

fn finish(size: u64, g: Vec<Candidate>) -> Option<DupeGroup> {
    let mut g = one_per_file(g);
    if g.len() < 2 {
        return None;
    }
    g.sort_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.path.cmp(&b.path)));
    Some(DupeGroup { size, files: g.into_iter().map(|c| DupeFile { path: c.path, modified: c.modified }).collect() })
}

fn pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 6))
        .thread_name(|i| format!("rf-dupes-{i}"))
        .start_handler(|_| {
            use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
            // SAFETY: changes only the priority of the calling thread, through its pseudo-handle.
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
        })
        .build()
        .expect("duplicate finder thread pool")
}

/// Finds the duplicates under `root` (blocking). `min_size`: smaller files are left out (at least 1: empty files never count).
pub fn find(root: &Path, min_size: u64, cancel: &AtomicBool, progress: &Progress) -> DupeResult {
    let tree = DiskTree::build(root, cancel, &progress.walk);
    let mut out = DupeResult { skipped_dirs: tree.skipped(), ..Default::default() };
    let min_size = min_size.max(1);
    let files: Vec<(u32, u64)> = (1..tree.len() as u32).filter(|&i| !tree.is_dir(i)).map(|i| (i, tree.size(i))).collect();
    out.files = files.len() as u64;
    let by_size: Vec<(u64, Vec<PathBuf>)> = shared(files.into_iter().filter(|&(_, s)| s >= min_size).collect(), |&(_, s)| s)
        .into_iter()
        .map(|g| (g[0].1, g.into_iter().map(|(i, _)| tree.path(i)).collect()))
        .collect();
    drop(tree);
    if cancel.load(Ordering::Relaxed) {
        out.cancelled = true;
        return out;
    }
    progress.phase.store(1, Ordering::Relaxed);
    progress.to_hash.store(by_size.iter().map(|(_, g)| g.len() as u64).sum(), Ordering::Relaxed);
    let unreadable = AtomicU64::new(0);
    let groups = Mutex::new(Vec::new());
    pool().install(|| {
        by_size.into_par_iter().for_each(|(size, paths)| {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let heads: Vec<Candidate> = paths
                .into_par_iter()
                .filter_map(|p| {
                    let c = read_head(p, size, cancel, &progress.bytes);
                    progress.hashed.fetch_add(1, Ordering::Relaxed);
                    if c.is_none() && !cancel.load(Ordering::Relaxed) {
                        unreadable.fetch_add(1, Ordering::Relaxed);
                    }
                    c
                })
                .collect();
            for g in shared(heads, |c| c.head) {
                // the head was the whole file, or what is left to compare is one file under several names
                let g = if size <= HEAD { g } else { one_per_file(g) };
                if size <= HEAD || g.len() < 2 {
                    if let Some(d) = finish(size, g) {
                        groups.lock().push(d);
                    }
                    continue;
                }
                let whole: Vec<([u8; 32], Candidate)> = g
                    .into_par_iter()
                    .filter_map(|c| match read_whole(&c, cancel, &progress.bytes) {
                        Some(h) => Some((h, c)),
                        None => {
                            if !cancel.load(Ordering::Relaxed) {
                                unreadable.fetch_add(1, Ordering::Relaxed);
                            }
                            None
                        }
                    })
                    .collect();
                for same in shared(whole, |(h, _)| *h) {
                    if let Some(d) = finish(size, same.into_iter().map(|(_, c)| c).collect()) {
                        groups.lock().push(d);
                    }
                }
            }
        });
    });
    let mut groups = groups.into_inner();
    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then_with(|| a.files[0].path.cmp(&b.files[0].path)));
    out.groups = groups;
    out.unreadable = unreadable.load(Ordering::Relaxed);
    out.cancelled = cancel.load(Ordering::Relaxed);
    progress.phase.store(2, Ordering::Relaxed);
    out
}

/// A scan running in the background. Dropping it (closing the tab) stops it.
pub struct DupeScan {
    cancel: Arc<AtomicBool>,
    progress: Arc<Progress>,
    result: Arc<Mutex<Option<DupeResult>>>,
}

impl DupeScan {
    pub fn start(root: PathBuf, min_size: u64) -> DupeScan {
        let scan = DupeScan { cancel: Arc::default(), progress: Arc::default(), result: Arc::default() };
        let (cancel, progress, result) = (scan.cancel.clone(), scan.progress.clone(), scan.result.clone());
        let _ = std::thread::Builder::new().name("rf-dupes".into()).spawn(move || {
            let r = find(&root, min_size, &cancel, &progress);
            *result.lock() = Some(r);
        });
        scan
    }

    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    /// The result once the scan has finished (taken: later calls return `None`).
    pub fn take(&self) -> Option<DupeResult> {
        self.result.lock().take()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for DupeScan {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn root(tag: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("rf-dupes-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&r);
        fs::create_dir_all(r.join("a/deep")).unwrap();
        fs::create_dir_all(r.join("b")).unwrap();
        r
    }

    fn names(g: &DupeGroup, root: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            g.files.iter().map(|f| f.path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/")).collect();
        v.sort();
        v
    }

    #[test]
    fn shared_keeps_groups_of_two_or_more() {
        let mut g = shared(vec![1, 2, 2, 3, 3, 3, 4], |&x| x);
        g.sort();
        assert_eq!(g, vec![vec![2, 2], vec![3, 3, 3]]);
    }

    #[test]
    fn finds_copies_and_tells_apart_files_of_one_size() {
        let r = root("basic");
        fs::write(r.join("one.txt"), "same contents").unwrap();
        fs::write(r.join("a/copy.txt"), "same contents").unwrap();
        fs::write(r.join("a/deep/again.dat"), "same contents").unwrap();
        fs::write(r.join("b/other.txt"), "diff contents").unwrap(); // the same size, other bytes
        fs::write(r.join("b/alone.txt"), "a size of its own").unwrap();
        fs::write(r.join("empty1"), "").unwrap();
        fs::write(r.join("b/empty2"), "").unwrap();

        let res = find(&r, 1, &AtomicBool::new(false), &Progress::default());
        assert!(!res.cancelled);
        assert_eq!(res.files, 7);
        assert_eq!(res.groups.len(), 1, "{:?}", res.groups);
        assert_eq!(names(&res.groups[0], &r), ["a/copy.txt", "a/deep/again.dat", "one.txt"]);
        assert_eq!(res.groups[0].size, 13);
        assert_eq!(res.wasted(), 26);
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn big_files_that_differ_only_after_the_head_are_not_duplicates() {
        let r = root("big");
        let mut a = vec![7u8; (HEAD as usize) * 3];
        fs::write(r.join("a.bin"), &a).unwrap();
        fs::write(r.join("b/a-copy.bin"), &a).unwrap();
        *a.last_mut().unwrap() = 8;
        fs::write(r.join("a/near.bin"), &a).unwrap();

        let res = find(&r, 1, &AtomicBool::new(false), &Progress::default());
        assert_eq!(res.groups.len(), 1);
        assert_eq!(names(&res.groups[0], &r), ["a.bin", "b/a-copy.bin"]);
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn hard_links_of_one_file_are_not_duplicates() {
        let r = root("hard");
        fs::write(r.join("file.txt"), "one file, two names").unwrap();
        fs::hard_link(r.join("file.txt"), r.join("b/link.txt")).unwrap();
        let res = find(&r, 1, &AtomicBool::new(false), &Progress::default());
        assert!(res.groups.is_empty(), "{:?}", res.groups);

        // a real copy beside them: one group of two files (the hard links count once)
        fs::write(r.join("a/copy.txt"), "one file, two names").unwrap();
        let res = find(&r, 1, &AtomicBool::new(false), &Progress::default());
        assert_eq!(res.groups.len(), 1);
        assert_eq!(res.groups[0].files.len(), 2);
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn small_files_can_be_left_out_and_the_biggest_waste_comes_first() {
        let r = root("order");
        fs::write(r.join("s1"), "tiny").unwrap();
        fs::write(r.join("b/s2"), "tiny").unwrap();
        fs::write(r.join("l1"), vec![1u8; 5000]).unwrap();
        fs::write(r.join("a/l2"), vec![1u8; 5000]).unwrap();

        let res = find(&r, 1, &AtomicBool::new(false), &Progress::default());
        assert_eq!(res.groups.len(), 2);
        assert_eq!(res.groups[0].size, 5000, "biggest waste first");
        let res = find(&r, 100, &AtomicBool::new(false), &Progress::default());
        assert_eq!(res.groups.len(), 1);
        assert_eq!(res.groups[0].size, 5000);
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn a_cancelled_scan_says_so() {
        let r = root("cancel");
        fs::write(r.join("x"), "x").unwrap();
        fs::write(r.join("b/x"), "x").unwrap();
        let res = find(&r, 1, &AtomicBool::new(true), &Progress::default());
        assert!(res.cancelled);
        assert!(res.groups.is_empty());
        let _ = fs::remove_dir_all(&r);
    }

    #[test]
    fn the_oldest_copy_comes_first() {
        let r = root("age");
        fs::write(r.join("new.txt"), "abc").unwrap();
        fs::write(r.join("b/old.txt"), "abc").unwrap();
        let old = SystemTime::now() - std::time::Duration::from_secs(86_400 * 30);
        File::options().write(true).open(r.join("b/old.txt")).unwrap().set_modified(old).unwrap();
        let res = find(&r, 1, &AtomicBool::new(false), &Progress::default());
        assert_eq!(res.groups[0].files[0].path, r.join("b/old.txt"));
        let _ = fs::remove_dir_all(&r);
    }
}
