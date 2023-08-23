use std::borrow::Cow;

use bstr::{ByteSlice, ByteVec};
/// 如果路径的最后一个部分是普通文件，则返回路径的最后一个组件。
///
/// 如果路径以 .、.. 结尾，或者仅由根或前缀组成，
/// 则 file_name 将返回 None。
pub fn file_name<'a>(path: &Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>> {
    if path.is_empty() {
        return None;
    } else if path.last_byte() == Some(b'.') {
        return None;
    }
    let last_slash = path.rfind_byte(b'/').map(|i| i + 1).unwrap_or(0);
    Some(match *path {
        Cow::Borrowed(path) => Cow::Borrowed(&path[last_slash..]),
        Cow::Owned(ref path) => {
            let mut path = path.clone();
            path.drain_bytes(..last_slash);
            Cow::Owned(path)
        }
    })
}

/// 给定路径的文件名，返回文件扩展名。
///
/// 请注意，这与 std::path::Path::extension 的语义不匹配。
/// 即，扩展名包括 `.`，匹配方式更自由。
/// 具体来说，扩展名是：
///
/// * 如果给定的文件名为空，则为 None；
/// * 如果没有嵌入的 `.`，则为 None；
/// * 否则，从最后一个 `.` 开始的文件名部分。
///
/// 例如，文件名 `.rs` 具有扩展名 `.rs`。
///
/// 注意：这是为了更容易进行某些 glob 匹配优化。
/// 例如，模式 `*.rs` 显然是要匹配具有 `rs` 扩展名的文件，
/// 但它也匹配没有扩展名的文件，如 `.rs`，根据 std::path::Path::extension，它没有扩展名。
pub fn file_name_ext<'a>(name: &Cow<'a, [u8]>) -> Option<Cow<'a, [u8]>> {
    if name.is_empty() {
        return None;
    }
    let last_dot_at = match name.rfind_byte(b'.') {
        None => return None,
        Some(i) => i,
    };
    Some(match *name {
        Cow::Borrowed(name) => Cow::Borrowed(&name[last_dot_at..]),
        Cow::Owned(ref name) => {
            let mut name = name.clone();
            name.drain_bytes(..last_dot_at);
            Cow::Owned(name)
        }
    })
}

/// 将路径规范化为在任何平台上都使用 `/` 作为分隔符，即使在识别其他字符作为分隔符的平台上也是如此。
#[cfg(unix)]
pub fn normalize_path(path: Cow<'_, [u8]>) -> Cow<'_, [u8]> {
    // UNIX 仅使用 /，所以我们没问题。
    path
}

/// 将路径规范化为在任何平台上都使用 `/` 作为分隔符，即使在识别其他字符作为分隔符的平台上也是如此。
#[cfg(not(unix))]
pub fn normalize_path(mut path: Cow<[u8]>) -> Cow<[u8]> {
    use std::path::is_separator;

    for i in 0..path.len() {
        if path[i] == b'/' || !is_separator(path[i] as char) {
            continue;
        }
        path.to_mut()[i] = b'/';
    }
    path
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use bstr::{ByteVec, B};

    use super::{file_name_ext, normalize_path};

    macro_rules! ext {
        ($name:ident, $file_name:expr, $ext:expr) => {
            #[test]
            fn $name() {
                let bs = Vec::from($file_name);
                let got = file_name_ext(&Cow::Owned(bs));
                assert_eq!($ext.map(|s| Cow::Borrowed(B(s))), got);
            }
        };
    }

    ext!(ext1, "foo.rs", Some(".rs"));
    ext!(ext2, ".rs", Some(".rs"));
    ext!(ext3, "..rs", Some(".rs"));
    ext!(ext4, "", None::<&str>);
    ext!(ext5, "foo", None::<&str>);

    macro_rules! normalize {
        ($name:ident, $path:expr, $expected:expr) => {
            #[test]
            fn $name() {
                let bs = Vec::from_slice($path);
                let got = normalize_path(Cow::Owned(bs));
                assert_eq!($expected.to_vec(), got.into_owned());
            }
        };
    }

    normalize!(normal1, b"foo", b"foo");
    normalize!(normal2, b"foo/bar", b"foo/bar");
    #[cfg(unix)]
    normalize!(normal3, b"foo\\bar", b"foo\\bar");
    #[cfg(not(unix))]
    normalize!(normal3, b"foo\\bar", b"foo/bar");
    #[cfg(unix)]
    normalize!(normal4, b"foo\\bar/baz", b"foo\\bar/baz");
    #[cfg(not(unix))]
    normalize!(normal4, b"foo\\bar/baz", b"foo/bar/baz");
}
