use std::ffi::OsStr;
use std::path::Path;

use crate::walk::DirEntry;

/// 当且仅当该条目被视为隐藏时，返回 true。
///
/// 这只在路径的基本名称以 `.` 开头时返回 true。
///
/// 在 Unix 上，这实现了更优化的检查。
#[cfg(unix)]
pub fn is_hidden(dent: &DirEntry) -> bool {
    use std::os::unix::ffi::OsStrExt;

    if let Some(name) = file_name(dent.path()) {
        name.as_bytes().get(0) == Some(&b'.')
    } else {
        false
    }
}

/// 当且仅当该条目被视为隐藏时，返回 true。
///
/// 在 Windows 上，如果满足以下条件之一，则返回 true：
///
/// * 路径的基本名称以 `.` 开头。
/// * 文件属性具有 `HIDDEN` 属性设置。
#[cfg(windows)]
pub fn is_hidden(dent: &DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    use winapi_util::file;

    // 这看起来像是我们正在进行额外的 stat 调用，但在 Windows 上，目录遍历器会重用从每个目录条目检索的元数据，并将其存储在 DirEntry 本身上。因此，这是“免费”的。
    if let Ok(md) = dent.metadata() {
        if file::is_hidden(md.file_attributes() as u64) {
            return true;
        }
    }
    if let Some(name) = file_name(dent.path()) {
        name.to_str().map(|s| s.starts_with(".")).unwrap_or(false)
    } else {
        false
    }
}

/// 当且仅当该条目被视为隐藏时，返回 true。
///
/// 这只在路径的基本名称以 `.` 开头时返回 true。
#[cfg(not(any(unix, windows)))]
pub fn is_hidden(dent: &DirEntry) -> bool {
    if let Some(name) = file_name(dent.path()) {
        name.to_str().map(|s| s.starts_with(".")).unwrap_or(false)
    } else {
        false
    }
}

/// 从 `path` 中剥离 `prefix` 并返回剩余部分。
///
/// 如果 `path` 没有前缀 `prefix`，则返回 `None`。
#[cfg(unix)]
pub fn strip_prefix<'a, P: AsRef<Path> + ?Sized>(
    prefix: &'a P,
    path: &'a Path,
) -> Option<&'a Path> {
    use std::os::unix::ffi::OsStrExt;

    let prefix = prefix.as_ref().as_os_str().as_bytes();
    let path = path.as_os_str().as_bytes();
    if prefix.len() > path.len() || prefix != &path[0..prefix.len()] {
        None
    } else {
        Some(&Path::new(OsStr::from_bytes(&path[prefix.len()..])))
    }
}

/// 从 `path` 中剥离 `prefix` 并返回剩余部分。
///
/// 如果 `path` 没有前缀 `prefix`，则返回 `None`。
#[cfg(not(unix))]
pub fn strip_prefix<'a, P: AsRef<Path> + ?Sized>(
    prefix: &'a P,
    path: &'a Path,
) -> Option<&'a Path> {
    path.strip_prefix(prefix).ok()
}

/// 如果此文件路径仅为文件名，则返回 true。即，其父目录为空字符串。
#[cfg(unix)]
pub fn is_file_name<P: AsRef<Path>>(path: P) -> bool {
    use memchr::memchr;
    use std::os::unix::ffi::OsStrExt;

    let path = path.as_ref().as_os_str().as_bytes();
    memchr(b'/', path).is_none()
}

/// 如果此文件路径仅为文件名，则返回 true。即，其父目录为空字符串。
#[cfg(not(unix))]
pub fn is_file_name<P: AsRef<Path>>(path: P) -> bool {
    path.as_ref().parent().map(|p| p.as_os_str().is_empty()).unwrap_or(false)
}

/// 路径的最终组件，如果它是正常文件的话。
///
/// 如果路径以 .、.. 结尾，或仅由前缀的根组成，file_name 将返回 None。
#[cfg(unix)]
pub fn file_name<'a, P: AsRef<Path> + ?Sized>(
    path: &'a P,
) -> Option<&'a OsStr> {
    use memchr::memrchr;
    use std::os::unix::ffi::OsStrExt;

    let path = path.as_ref().as_os_str().as_bytes();
    if path.is_empty() {
        return None;
    } else if path.len() == 1 && path[0] == b'.' {
        return None;
    } else if path.last() == Some(&b'.') {
        return None;
    } else if path.len() >= 2 && &path[path.len() - 2..] == &b".."[..] {
        return None;
    }
    let last_slash = memrchr(b'/', path).map(|i| i + 1).unwrap_or(0);
    Some(OsStr::from_bytes(&path[last_slash..]))
}

/// 路径的最终组件，如果它是正常文件的话。
///
/// 如果路径以 .、.. 结尾，或仅由前缀的根组成，file_name 将返回 None。
#[cfg(not(unix))]
pub fn file_name<'a, P: AsRef<Path> + ?Sized>(
    path: &'a P,
) -> Option<&'a OsStr> {
    path.as_ref().file_name()
}
