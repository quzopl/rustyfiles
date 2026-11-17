//! Comparing two folder trees (the "Compare with the other pane" view).
//!
//! Both trees are walked (reparse points are listed but not followed) and matched by relative path, case-insensitively as
//! Windows names are. A file is the same on both sides when its size matches and its modification times are within
//! [`TIME_TOLERANCE`] (FAT and exFAT keep times to two seconds). A folder that is only on one side is one entry: copying it
//! copies everything in it, so its contents are not listed. Blocking: [`CompareScan`] runs it on a thread of its own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

/// Modification times closer than this are the same time.
pub const TIME_TOLERANCE: Duration = Duration::from_secs(2);

/// One side of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Side {
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// How the two sides of an entry differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Diff {
    /// Only in the left tree (a folder: with everything in it).
    OnlyLeft,
    /// Only in the right tree.
    OnlyRight,
    /// On both sides, the left one changed later.
    LeftNewer,
    /// On both sides, the right one changed later.
    RightNewer,
    /// On both sides with the same time but another size, or a file on one side and a folder on the other.
    Conflict,
    /// The same size and time.
    Same,
}

/// One relative path and what each side has there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Relative to both roots, `\`-separated, as the left side spells it (the right side's when only there).
    pub rel: PathBuf,
    pub left: Option<Side>,
    pub right: Option<Side>,
    pub diff: Diff,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.left.or(self.right).is_some_and(|s| s.is_dir)
    }
}

/// What a finished (or cancelled) comparison found.
#[derive(Debug, Default)]
pub struct CompareResult {
    /// In path order; identical files included (the view hides them by default).
    pub entries: Vec<Entry>,
    /// Folders that could not be listed on either side.
    pub unreadable: u64,
    pub cancelled: bool,
}

impl CompareResult {
    pub fn count(&self, d: Diff) -> usize {
        self.entries.iter().filter(|e| e.diff == d).count()
    }
}

/// Decides how two sides of one path differ.
pub fn diff_of(left: Option<&Side>, right: Option<&Side>) -> Diff {
    match (left, right) {
        (Some(_), None) => Diff::OnlyLeft,
        (None, Some(_)) => Diff::OnlyRight,
        (None, None) => Diff::Same,
        (Some(l), Some(r)) if l.is_dir != r.is_dir => Diff::Conflict,
        (Some(l), Some(_)) if l.is_dir => Diff::Same, // two folders: their contents are entries of their own
        (Some(l), Some(r)) => {
            let newer = match (l.modified, r.modified) {
                (Some(a), Some(b)) if a > b + TIME_TOLERANCE => Some(Diff::LeftNewer),
                (Some(a), Some(b)) if b > a + TIME_TOLERANCE => Some(Diff::RightNewer),
                _ => None,
            };
            match newer {
                Some(d) => d,
                None if l.size == r.size => Diff::Same,
                None => Diff::Conflict,
            }
        }
    }
}

/// Lists one folder: (name, side) per entry, reparse points as entries that are not walked into. `None` if unreadable.
fn list(dir: &Path) -> Option<Vec<(String, Side, bool)>> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let rd = std::fs::read_dir(dir).ok()?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let Ok(m) = e.metadata() else { continue };
        let reparse = m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        let is_dir = m.is_dir();
        out.push((
            e.file_name().to_string_lossy().into_owned(),
            Side { is_dir, size: if is_dir { 0 } else { m.len() }, modified: m.modified().ok() },
            is_dir && !reparse,
        ));
    }
    Some(out)
}

struct Walk<'a> {
    left: &'a Path,
    right: &'a Path,
    cancel: &'a AtomicBool,
    seen: &'a AtomicU64,
    unreadable: &'a AtomicU64,
}

/// Compares the folder `rel` (present on both sides) and recurses into the folders that are on both sides.
fn walk(w: &Walk, rel: &Path, out: &mut Vec<Entry>) {
    if w.cancel.load(Ordering::Relaxed) {
        return;
    }
    let (l, r) = rayon::join(|| list(&w.left.join(rel)), || list(&w.right.join(rel)));
    if l.is_none() || r.is_none() {
        w.unreadable.fetch_add(1, Ordering::Relaxed);
    }
    // matched by the lowercase name: one map key per name, whichever side has it
    let mut by: BTreeMap<String, (Option<(String, Side, bool)>, Option<(String, Side, bool)>)> = BTreeMap::new();
    for it in l.unwrap_or_default() {
        let key = it.0.to_lowercase();
        by.entry(key).or_default().0 = Some(it);
    }
    for it in r.unwrap_or_default() {
        let key = it.0.to_lowercase();
        by.entry(key).or_default().1 = Some(it);
    }
    w.seen.fetch_add(by.len() as u64, Ordering::Relaxed);
    let mut sub = Vec::new();
    for (_, (lo, ro)) in by {
        let name = lo.as_ref().or(ro.as_ref()).map(|x| x.0.clone()).unwrap_or_default();
        let rel_name = rel.join(&name);
        let d = diff_of(lo.as_ref().map(|x| &x.1), ro.as_ref().map(|x| &x.1));
        let both_walkable = lo.as_ref().is_some_and(|x| x.2) && ro.as_ref().is_some_and(|x| x.2);
        if both_walkable {
            sub.push(rel_name);
            continue;
        }
        out.push(Entry { rel: rel_name, left: lo.map(|x| x.1), right: ro.map(|x| x.1), diff: d });
    }
    for s in sub {
        walk(w, &s, out);
    }
}

/// Compares `left` with `right` (blocking). `seen` counts the names looked at, for a progress line.
pub fn compare(left: &Path, right: &Path, cancel: &AtomicBool, seen: &AtomicU64) -> CompareResult {
    let unreadable = AtomicU64::new(0);
    let w = Walk { left, right, cancel, seen, unreadable: &unreadable };
    let mut entries = Vec::new();
    walk(&w, Path::new(""), &mut entries);
    entries.sort_by(|a, b| a.rel.to_string_lossy().to_lowercase().cmp(&b.rel.to_string_lossy().to_lowercase()));
    CompareResult { entries, unreadable: unreadable.load(Ordering::Relaxed), cancelled: cancel.load(Ordering::Relaxed) }
}

/// Which way to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ToRight,
    ToLeft,
}

/// The copies that bring `entries` over in `dir`: (source, destination folder) per entry that has a source on that side.
/// Grouped by destination folder, as one copy operation per folder.
pub fn copies<'a>(left: &Path, right: &Path, entries: impl IntoIterator<Item = &'a Entry>, dir: Direction) -> BTreeMap<PathBuf, Vec<PathBuf>> {
    let (from, to) = match dir {
        Direction::ToRight => (left, right),
        Direction::ToLeft => (right, left),
    };
    let mut out: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for e in entries {
        let has_source = match dir {
            Direction::ToRight => e.left.is_some(),
            Direction::ToLeft => e.right.is_some(),
        };
        if !has_source {
            continue;
        }
        let dest = e.rel.parent().map_or_else(|| to.to_path_buf(), |p| to.join(p));
        out.entry(dest).or_default().push(from.join(&e.rel));
    }
    out
}

/// A comparison running in the background. Dropping it (closing the tab) stops it.
pub struct CompareScan {
    cancel: Arc<AtomicBool>,
    seen: Arc<AtomicU64>,
    result: Arc<Mutex<Option<CompareResult>>>,
}

impl CompareScan {
    pub fn start(left: PathBuf, right: PathBuf) -> CompareScan {
        let scan = CompareScan { cancel: Arc::default(), seen: Arc::default(), result: Arc::default() };
        let (cancel, seen, result) = (scan.cancel.clone(), scan.seen.clone(), scan.result.clone());
        let _ = std::thread::Builder::new().name("rf-foldercmp".into()).spawn(move || {
            let r = compare(&left, &right, &cancel, &seen);
            *result.lock() = Some(r);
        });
        scan
    }

    /// Names looked at so far.
    pub fn seen(&self) -> u64 {
        self.seen.load(Ordering::Relaxed)
    }

    /// The result once the comparison has finished (taken: later calls return `None`).
    pub fn take(&self) -> Option<CompareResult> {
        self.result.lock().take()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for CompareScan {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn file(size: u64, secs: u64) -> Side {
        Side { is_dir: false, size, modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)) }
    }

    #[test]
    fn sides_are_told_apart() {
        let dir = Side { is_dir: true, size: 0, modified: None };
        assert_eq!(diff_of(Some(&file(1, 100)), None), Diff::OnlyLeft);
        assert_eq!(diff_of(None, Some(&file(1, 100))), Diff::OnlyRight);
        assert_eq!(diff_of(Some(&file(5, 100)), Some(&file(5, 100))), Diff::Same);
        assert_eq!(diff_of(Some(&file(5, 100)), Some(&file(5, 101))), Diff::Same, "within the tolerance (FAT keeps two seconds)");
        assert_eq!(diff_of(Some(&file(5, 200)), Some(&file(9, 100))), Diff::LeftNewer);
        assert_eq!(diff_of(Some(&file(5, 100)), Some(&file(5, 200))), Diff::RightNewer);
        assert_eq!(diff_of(Some(&file(5, 100)), Some(&file(6, 100))), Diff::Conflict, "same time, other size");
        assert_eq!(diff_of(Some(&dir), Some(&file(6, 100))), Diff::Conflict, "a folder against a file");
        assert_eq!(diff_of(Some(&dir), Some(&dir)), Diff::Same);
    }

    fn tree(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("rf-foldercmp-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (l, r) = (base.join("L"), base.join("R"));
        fs::create_dir_all(l.join("sub")).unwrap();
        fs::create_dir_all(r.join("SUB")).unwrap(); // the same folder, another case
        (base, l, r)
    }

    fn set_age(p: &Path, secs_ago: u64) {
        let t = SystemTime::now() - Duration::from_secs(secs_ago);
        fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    fn rels(res: &CompareResult, d: Diff) -> Vec<String> {
        res.entries.iter().filter(|e| e.diff == d).map(|e| e.rel.to_string_lossy().replace('\\', "/")).collect()
    }

    #[test]
    fn two_trees_are_compared_by_path_size_and_time() {
        let (base, l, r) = tree("basic");
        fs::write(l.join("same.txt"), "x").unwrap();
        fs::write(r.join("same.txt"), "x").unwrap();
        set_age(&l.join("same.txt"), 1000);
        set_age(&r.join("same.txt"), 1000);
        fs::write(l.join("left-only.txt"), "l").unwrap();
        fs::write(r.join("SUB").join("right-only.txt"), "r").unwrap();
        fs::write(l.join("sub").join("changed.txt"), "new contents").unwrap();
        fs::write(r.join("SUB").join("changed.txt"), "old").unwrap();
        set_age(&r.join("SUB").join("changed.txt"), 5000);
        fs::create_dir_all(l.join("only-here").join("deep")).unwrap();
        fs::write(l.join("only-here").join("deep").join("a.txt"), "a").unwrap();

        let res = compare(&l, &r, &AtomicBool::new(false), &AtomicU64::new(0));
        assert!(!res.cancelled);
        assert_eq!(rels(&res, Diff::OnlyLeft), ["left-only.txt", "only-here"], "a folder only on one side is one entry");
        assert_eq!(rels(&res, Diff::OnlyRight), ["sub/right-only.txt"], "matched case-insensitively, spelled as the left has it");
        assert_eq!(rels(&res, Diff::LeftNewer), ["sub/changed.txt"]);
        assert_eq!(rels(&res, Diff::Same), ["same.txt"]);
        assert!(res.entries.iter().find(|e| e.rel == Path::new("only-here")).unwrap().is_dir());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn copies_go_into_the_matching_folder_of_the_other_side() {
        let e = |rel: &str, l: bool, r: bool| Entry {
            rel: PathBuf::from(rel),
            left: l.then(|| file(1, 1)),
            right: r.then(|| file(1, 1)),
            diff: Diff::Same,
        };
        let entries = [e("a.txt", true, false), e(r"sub\b.txt", true, true), e(r"sub\c.txt", true, false), e("r.txt", false, true)];
        let (l, r) = (Path::new(r"C:\L"), Path::new(r"D:\R"));
        let to_right = copies(l, r, &entries, Direction::ToRight);
        assert_eq!(to_right.len(), 2);
        assert_eq!(to_right[Path::new(r"D:\R")], [PathBuf::from(r"C:\L\a.txt")]);
        assert_eq!(to_right[Path::new(r"D:\R\sub")], [PathBuf::from(r"C:\L\sub\b.txt"), PathBuf::from(r"C:\L\sub\c.txt")]);
        let to_left = copies(l, r, &entries, Direction::ToLeft);
        assert_eq!(to_left[Path::new(r"C:\L")], [PathBuf::from(r"D:\R\r.txt")], "only what the right side has");
        assert_eq!(to_left[Path::new(r"C:\L\sub")], [PathBuf::from(r"D:\R\sub\b.txt")]);
    }

    #[test]
    fn a_cancelled_comparison_says_so() {
        let (base, l, r) = tree("cancel");
        fs::write(l.join("x"), "x").unwrap();
        let res = compare(&l, &r, &AtomicBool::new(true), &AtomicU64::new(0));
        assert!(res.cancelled);
        assert!(res.entries.is_empty());
        let _ = fs::remove_dir_all(&base);
    }
}
