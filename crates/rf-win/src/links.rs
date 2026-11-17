//! Symbolic links, hard links and junctions made from the file manager ("Paste as link").
//!
//! Errors come back as [`LinkError`], not as text: the app words them in the user's language.
//! Blocking (file system calls): call off the UI thread.

use std::io;
use std::path::{Path, PathBuf};

/// What kind of link to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkKind {
    /// A symbolic link to a file or a folder (`mklink` / `mklink /D`). Needs Developer Mode or administrator rights.
    Symbolic,
    /// A hard link: a second name of the same file on the same volume (`mklink /H`). Files only.
    Hard,
    /// A junction: a folder that stands for another local folder (`mklink /J`). Folders only, no rights needed.
    Junction,
}

/// Why a link could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// The source does not exist (any more).
    SourceMissing,
    /// A symbolic link needs Developer Mode or administrator rights (ERROR_PRIVILEGE_NOT_HELD).
    NeedsPrivilege,
    /// A hard link must be on the same volume as its file (ERROR_NOT_SAME_DEVICE).
    DifferentVolume,
    /// A hard link can only be made to a file.
    HardLinkToFolder,
    /// A junction can only point to a folder.
    JunctionToFile,
    /// A junction can only point to a folder on a local drive, not to a network path.
    JunctionNotLocal,
    /// The file system of the target folder can't hold this kind of link (FAT32, exFAT, some network shares).
    Unsupported,
    /// Any other failure, with the system's own text.
    Other(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::SourceMissing => f.write_str("the source does not exist"),
            LinkError::NeedsPrivilege => f.write_str("symbolic links need Developer Mode or administrator rights"),
            LinkError::DifferentVolume => f.write_str("a hard link must be on the same volume as its file"),
            LinkError::HardLinkToFolder => f.write_str("a hard link can only be made to a file"),
            LinkError::JunctionToFile => f.write_str("a junction can only point to a folder"),
            LinkError::JunctionNotLocal => f.write_str("a junction can only point to a folder on a local drive"),
            LinkError::Unsupported => f.write_str("this file system does not support this kind of link"),
            LinkError::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for LinkError {}

const ERROR_INVALID_FUNCTION: i32 = 1;
const ERROR_NOT_SAME_DEVICE: i32 = 17;
const ERROR_NOT_SUPPORTED: i32 = 50;
const ERROR_INVALID_REPARSE_DATA: i32 = 4392;
const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;

fn from_io(e: io::Error) -> LinkError {
    match e.raw_os_error() {
        Some(ERROR_PRIVILEGE_NOT_HELD) => LinkError::NeedsPrivilege,
        Some(ERROR_NOT_SAME_DEVICE) => LinkError::DifferentVolume,
        Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_REPARSE_DATA) => LinkError::Unsupported,
        _ if e.kind() == io::ErrorKind::NotFound => LinkError::SourceMissing,
        _ => LinkError::Other(e.to_string()),
    }
}

/// Whether anything (a file, a folder, or a link, even a broken one) has this name. `Path::exists` follows links and says
/// "no" for a broken one, which would make a second link try the name of the first.
fn taken(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// The name for a link to `source` in `dir`: the source's own name, or "name (2).ext", "name (3).ext"… when that is taken.
/// A folder's name is not split at a dot. `None` when the source has no name (a drive root such as `C:\`).
pub fn link_name(dir: &Path, source: &Path, is_dir: bool, taken: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let name = source.file_name()?.to_string_lossy().into_owned();
    let first = dir.join(&name);
    if !taken(&first) {
        return Some(first);
    }
    let (stem, ext) = match name.rfind('.') {
        // ".gitignore" has no extension; "a.b.txt" keeps ".txt"
        Some(i) if !is_dir && i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.clone(), String::new()),
    };
    (2u32..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !taken(p))
}

/// Checks what can be known before touching the disk: the kind of link against what the source is.
pub fn check(kind: LinkKind, source_is_dir: bool, source: &Path) -> Result<(), LinkError> {
    match kind {
        LinkKind::Hard if source_is_dir => Err(LinkError::HardLinkToFolder),
        LinkKind::Junction if !source_is_dir => Err(LinkError::JunctionToFile),
        LinkKind::Junction if junction_target(source).is_none() => Err(LinkError::JunctionNotLocal),
        _ => Ok(()),
    }
}

/// The absolute local path a junction can point to: `C:\x`, also given as `\\?\C:\x`. `None` for a network path
/// (`\\server\share`, `\\?\UNC\…`), a relative path or a device path.
pub fn junction_target(p: &Path) -> Option<String> {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let b = s.as_bytes();
    let drive_abs = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    drive_abs.then(|| s.replace('/', "\\"))
}

/// Makes a link of `kind` to `source` in `dir` and returns its path. The name is the source's, numbered when taken.
pub fn create_in(kind: LinkKind, source: &Path, dir: &Path) -> Result<PathBuf, LinkError> {
    // follows a link given as the source: a link to a link is made to what the first one is
    let meta = std::fs::metadata(source).map_err(from_io)?;
    let is_dir = meta.is_dir();
    check(kind, is_dir, source)?;
    let link = link_name(dir, source, is_dir, taken).ok_or_else(|| LinkError::Other(format!("no name for {}", source.display())))?;
    create(kind, source, &link, is_dir)?;
    Ok(link)
}

/// Makes the link `link` to `source`. `link` must not exist.
pub fn create(kind: LinkKind, source: &Path, link: &Path, source_is_dir: bool) -> Result<(), LinkError> {
    check(kind, source_is_dir, source)?;
    match kind {
        LinkKind::Symbolic if source_is_dir => std::os::windows::fs::symlink_dir(source, link).map_err(from_io),
        LinkKind::Symbolic => std::os::windows::fs::symlink_file(source, link).map_err(from_io),
        LinkKind::Hard => std::fs::hard_link(source, link).map_err(from_io),
        LinkKind::Junction => {
            let target = junction_target(source).ok_or(LinkError::JunctionNotLocal)?;
            make_junction(&target, link)
        }
    }
}

/// The REPARSE_DATA_BUFFER of a mount point (junction) to the absolute local path `target` ("C:\x"):
/// tag, data length, reserved, then the substitute name ("\??\C:\x") and the print name ("C:\x"), each NUL-terminated.
fn junction_buffer(target: &str) -> Vec<u8> {
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    let target = target.trim_end_matches('\\');
    // a drive root keeps its backslash ("C:\"), and so does the substitute name
    let target = if target.len() == 2 { format!("{target}\\") } else { target.to_string() };
    let subst: Vec<u16> = format!(r"\??\{target}").encode_utf16().collect();
    let print: Vec<u16> = target.encode_utf16().collect();
    let subst_len = (subst.len() * 2) as u16;
    let print_len = (print.len() * 2) as u16;
    // the path buffer: subst + NUL + print + NUL
    let path_bytes = subst_len as usize + 2 + print_len as usize + 2;
    let data_len = (8 + path_bytes) as u16; // the four u16 offsets/lengths, then the paths
    let mut buf = Vec::with_capacity(8 + data_len as usize);
    buf.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buf.extend_from_slice(&data_len.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
    buf.extend_from_slice(&0u16.to_le_bytes()); // substitute name offset
    buf.extend_from_slice(&subst_len.to_le_bytes());
    buf.extend_from_slice(&(subst_len + 2).to_le_bytes()); // print name offset
    buf.extend_from_slice(&print_len.to_le_bytes());
    for c in subst.iter().chain([&0u16]).chain(print.iter()).chain([&0u16]) {
        buf.extend_from_slice(&c.to_le_bytes());
    }
    buf
}

/// Makes the junction `link` (a new, empty folder turned into a mount point) to the absolute local folder `target`.
fn make_junction(target: &str, link: &Path) -> Result<(), LinkError> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_NONE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
    use windows::core::PCWSTR;

    std::fs::create_dir(link).map_err(from_io)?;
    let w = crate::wide::wide(link);
    let buf = junction_buffer(target);
    // SAFETY: `w` is a NUL-terminated UTF-16 path that outlives the call; no security attributes or template handle.
    let h = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            0x4000_0000, // GENERIC_WRITE
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, // a folder, and the reparse point itself
            None,
        )
    };
    let h = match h {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir(link);
            return Err(from_io(win_err(e)));
        }
    };
    let mut ret = 0u32;
    // SAFETY: `h` is an open handle to the new folder; the input buffer is `buf.len()` bytes and lives across the call; no output.
    let r = unsafe { DeviceIoControl(h, FSCTL_SET_REPARSE_POINT, Some(buf.as_ptr() as *const _), buf.len() as u32, None, 0, Some(&mut ret), None) };
    // SAFETY: `h` was opened above and is closed once.
    let _ = unsafe { CloseHandle(h) };
    if let Err(e) = r {
        // the folder is still an ordinary empty one: take it away again
        let _ = std::fs::remove_dir(link);
        return Err(from_io(win_err(e)));
    }
    Ok(())
}

/// A Win32 error carried in an HRESULT (0x8007xxxx) back to an `io::Error` with its own code.
fn win_err(e: windows::core::Error) -> io::Error {
    let hr = e.code().0 as u32;
    if hr & 0xFFFF_0000 == 0x8007_0000 { io::Error::from_raw_os_error((hr & 0xFFFF) as i32) } else { io::Error::other(e.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn taken_of(names: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = names.iter().map(|n| Path::new(r"C:\d").join(n)).collect();
        move |p| set.contains(p)
    }

    #[test]
    fn the_name_is_the_sources_and_numbered_when_taken() {
        let dir = Path::new(r"C:\d");
        assert_eq!(link_name(dir, Path::new(r"E:\x\photo.jpg"), false, taken_of(&[])), Some(dir.join("photo.jpg")));
        assert_eq!(link_name(dir, Path::new(r"E:\x\photo.jpg"), false, taken_of(&["photo.jpg"])), Some(dir.join("photo (2).jpg")));
        assert_eq!(
            link_name(dir, Path::new(r"E:\x\photo.jpg"), false, taken_of(&["photo.jpg", "photo (2).jpg"])),
            Some(dir.join("photo (3).jpg"))
        );
        assert_eq!(link_name(dir, Path::new(r"E:\x\a.b.txt"), false, taken_of(&["a.b.txt"])), Some(dir.join("a.b (2).txt")));
        assert_eq!(link_name(dir, Path::new(r"E:\x\.gitignore"), false, taken_of(&[".gitignore"])), Some(dir.join(".gitignore (2)")));
    }

    #[test]
    fn a_folders_name_is_not_split_at_a_dot() {
        let dir = Path::new(r"C:\d");
        assert_eq!(link_name(dir, Path::new(r"E:\x\v1.2"), true, taken_of(&["v1.2"])), Some(dir.join("v1.2 (2)")));
    }

    #[test]
    fn a_drive_root_has_no_name() {
        assert_eq!(link_name(Path::new(r"C:\d"), Path::new(r"E:\"), true, taken_of(&[])), None);
    }

    #[test]
    fn kinds_are_checked_against_the_source() {
        let dir = Path::new(r"C:\x");
        assert_eq!(check(LinkKind::Hard, true, dir), Err(LinkError::HardLinkToFolder));
        assert_eq!(check(LinkKind::Hard, false, dir), Ok(()));
        assert_eq!(check(LinkKind::Junction, false, dir), Err(LinkError::JunctionToFile));
        assert_eq!(check(LinkKind::Junction, true, dir), Ok(()));
        assert_eq!(check(LinkKind::Junction, true, Path::new(r"\\server\share\x")), Err(LinkError::JunctionNotLocal));
        assert_eq!(check(LinkKind::Symbolic, true, Path::new(r"\\server\share\x")), Ok(()));
    }

    #[test]
    fn junction_targets_are_local_absolute_paths() {
        assert_eq!(junction_target(Path::new(r"C:\a\b")), Some(r"C:\a\b".into()));
        assert_eq!(junction_target(Path::new(r"\\?\D:\a")), Some(r"D:\a".into()));
        assert_eq!(junction_target(Path::new("C:/a/b")), Some(r"C:\a\b".into()));
        assert_eq!(junction_target(Path::new(r"\\server\share")), None);
        assert_eq!(junction_target(Path::new(r"\\?\UNC\server\share")), None);
        assert_eq!(junction_target(Path::new(r"a\b")), None);
        assert_eq!(junction_target(Path::new(r"C:a")), None);
    }

    #[test]
    fn the_junction_buffer_is_laid_out_as_windows_reads_it() {
        let b = junction_buffer(r"C:\x");
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), 0xA000_0003);
        let subst = r"\??\C:\x".len() as u16 * 2;
        let print = r"C:\x".len() as u16 * 2;
        assert_eq!(u16_at(4) as usize, b.len() - 8, "data length counts everything after the 8-byte header");
        assert_eq!((u16_at(8), u16_at(10), u16_at(12), u16_at(14)), (0, subst, subst + 2, print));
        let path: Vec<u16> = b[16..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16_lossy(&path), "\\??\\C:\\x\0C:\\x\0");
        // a drive root keeps its backslash
        let root = junction_buffer(r"D:\");
        let path: Vec<u16> = root[16..].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16_lossy(&path), "\\??\\D:\\\0D:\\\0");
    }

    #[test]
    fn os_errors_become_reasons() {
        assert_eq!(from_io(io::Error::from_raw_os_error(ERROR_PRIVILEGE_NOT_HELD)), LinkError::NeedsPrivilege);
        assert_eq!(from_io(io::Error::from_raw_os_error(ERROR_NOT_SAME_DEVICE)), LinkError::DifferentVolume);
        assert_eq!(from_io(io::Error::from_raw_os_error(ERROR_NOT_SUPPORTED)), LinkError::Unsupported);
        assert_eq!(from_io(io::Error::from_raw_os_error(2)), LinkError::SourceMissing);
        assert_eq!(win_err(windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_0522u32 as i32))).raw_os_error(), Some(1314));
    }

    /// On the real disk: a junction and a hard link (neither needs rights), and a symbolic link where Windows allows one.
    #[test]
    fn links_are_made_on_disk() {
        let base = std::env::temp_dir().join(format!("rf-links-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        let out = base.join("out");
        std::fs::create_dir_all(src.join("folder")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(src.join("folder").join("inside.txt"), "in").unwrap();
        std::fs::write(src.join("file.txt"), "hello").unwrap();

        let j = create_in(LinkKind::Junction, &src.join("folder"), &out).unwrap();
        assert_eq!(j, out.join("folder"));
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        assert!(std::fs::symlink_metadata(&j).unwrap().file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0, "a reparse point");
        assert_eq!(std::fs::read_to_string(j.join("inside.txt")).unwrap(), "in", "the junction shows the folder");

        let h = create_in(LinkKind::Hard, &src.join("file.txt"), &out).unwrap();
        assert_eq!(std::fs::read_to_string(&h).unwrap(), "hello");
        let h2 = create_in(LinkKind::Hard, &src.join("file.txt"), &out).unwrap();
        assert_eq!(h2, out.join("file (2).txt"), "a second link takes the next name");

        assert_eq!(create_in(LinkKind::Hard, &src.join("folder"), &out), Err(LinkError::HardLinkToFolder));
        assert_eq!(create_in(LinkKind::Junction, &src.join("file.txt"), &out), Err(LinkError::JunctionToFile));
        assert_eq!(create_in(LinkKind::Hard, &src.join("missing.txt"), &out), Err(LinkError::SourceMissing));

        match create_in(LinkKind::Symbolic, &src.join("file.txt"), &base) {
            Ok(s) => assert_eq!(std::fs::read_to_string(s).unwrap(), "hello"),
            Err(LinkError::NeedsPrivilege) => eprintln!("no Developer Mode here: the symbolic link was refused, as expected"),
            Err(e) => panic!("symbolic link: {e}"),
        }

        // removing the junction leaves what it points to
        std::fs::remove_dir(&j).unwrap();
        assert!(src.join("folder").join("inside.txt").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
