use std::str;

use memchr::memchr;
/// 在`replacement`中插入捕获引用，并将插入结果写入`dst`。`replacement`中的引用采用$N或$name的形式，
/// 其中`N`是捕获组索引，`name`是捕获组名称。提供的函数`name_to_index`将捕获组名称映射到索引。
///
/// 给定的`append`函数负责将替换写入`dst`缓冲区。也就是说，它会接收捕获组索引，并将其解析为相应的匹配文本。
/// 如果不存在这样的匹配文本，则`append`不应向其给定的缓冲区写入任何内容。
pub fn interpolate<A, N>(
    mut replacement: &[u8],
    mut append: A,
    mut name_to_index: N,
    dst: &mut Vec<u8>,
) where
    A: FnMut(usize, &mut Vec<u8>),
    N: FnMut(&str) -> Option<usize>,
{
    while !replacement.is_empty() {
        match memchr(b'$', replacement) {
            None => break,
            Some(i) => {
                dst.extend(&replacement[..i]);
                replacement = &replacement[i..];
            }
        }
        if replacement.get(1).map_or(false, |&b| b == b'$') {
            dst.push(b'$');
            replacement = &replacement[2..];
            continue;
        }
        debug_assert!(!replacement.is_empty());
        let cap_ref = match find_cap_ref(replacement) {
            Some(cap_ref) => cap_ref,
            None => {
                dst.push(b'$');
                replacement = &replacement[1..];
                continue;
            }
        };
        replacement = &replacement[cap_ref.end..];
        match cap_ref.cap {
            Ref::Number(i) => append(i, dst),
            Ref::Named(name) => {
                if let Some(i) = name_to_index(name) {
                    append(i, dst);
                }
            }
        }
    }
    dst.extend(replacement);
}

/// `CaptureRef` 表示文本中的捕获组引用。引用可以是捕获组名称或数字。
///
/// 它还带有紧随捕获引用的文本位置标记。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaptureRef<'a> {
    cap: Ref<'a>,
    end: usize,
}

/// 文本中捕获组的引用。
///
/// 例如，`$2`、`$foo`、`${foo}`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Ref<'a> {
    Named(&'a str),
    Number(usize),
}

impl<'a> From<&'a str> for Ref<'a> {
    fn from(x: &'a str) -> Ref<'a> {
        Ref::Named(x)
    }
}

impl From<usize> for Ref<'static> {
    fn from(x: usize) -> Ref<'static> {
        Ref::Number(x)
    }
}

/// 在给定文本中解析可能的捕获组名称引用，从 `replacement` 的开头开始。
///
/// 如果找不到有效的引用，将返回 None。
fn find_cap_ref(replacement: &[u8]) -> Option<CaptureRef<'_>> {
    let mut i = 0;
    if replacement.len() <= 1 || replacement[0] != b'$' {
        return None;
    }
    let mut brace = false;
    i += 1;
    if replacement[i] == b'{' {
        brace = true;
        i += 1;
    }
    let mut cap_end = i;
    while replacement.get(cap_end).map_or(false, is_valid_cap_letter) {
        cap_end += 1;
    }
    if cap_end == i {
        return None;
    }
    // 我们刚刚验证了范围 0..cap_end 是有效的 ASCII，因此它必须是有效的 UTF-8。
    // 如果我们真的关心的话，我们可以使用未检查的转换或直接从 &[u8] 解析数字来避免此 UTF-8 检查。
    let cap = str::from_utf8(&replacement[i..cap_end])
        .expect("valid UTF-8 capture name");
    if brace {
        if !replacement.get(cap_end).map_or(false, |&b| b == b'}') {
            return None;
        }
        cap_end += 1;
    }
    Some(CaptureRef {
        cap: match cap.parse::<u32>() {
            Ok(i) => Ref::Number(i as usize),
            Err(_) => Ref::Named(cap),
        },
        end: cap_end,
    })
}

/// 仅当给定字节在捕获名称中允许时返回 true。
fn is_valid_cap_letter(b: &u8) -> bool {
    match *b {
        b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'_' => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{find_cap_ref, interpolate, CaptureRef};

    macro_rules! find {
        ($name:ident, $text:expr) => {
            #[test]
            fn $name() {
                assert_eq!(None, find_cap_ref($text.as_bytes()));
            }
        };
        ($name:ident, $text:expr, $capref:expr) => {
            #[test]
            fn $name() {
                assert_eq!(Some($capref), find_cap_ref($text.as_bytes()));
            }
        };
    }

    macro_rules! c {
        ($name_or_number:expr, $pos:expr) => {
            CaptureRef { cap: $name_or_number.into(), end: $pos }
        };
    }

    find!(find_cap_ref1, "$foo", c!("foo", 4));
    find!(find_cap_ref2, "${foo}", c!("foo", 6));
    find!(find_cap_ref3, "$0", c!(0, 2));
    find!(find_cap_ref4, "$5", c!(5, 2));
    find!(find_cap_ref5, "$10", c!(10, 3));
    find!(find_cap_ref6, "$42a", c!("42a", 4));
    find!(find_cap_ref7, "${42}a", c!(42, 5));
    find!(find_cap_ref8, "${42");
    find!(find_cap_ref9, "${42 ");
    find!(find_cap_ref10, " $0 ");
    find!(find_cap_ref11, "$");
    find!(find_cap_ref12, " ");
    find!(find_cap_ref13, "");

    // A convenience routine for using interpolate's unwieldy but flexible API.
    fn interpolate_string(
        mut name_to_index: Vec<(&'static str, usize)>,
        caps: Vec<&'static str>,
        replacement: &str,
    ) -> String {
        name_to_index.sort_by_key(|x| x.0);

        let mut dst = vec![];
        interpolate(
            replacement.as_bytes(),
            |i, dst| {
                if let Some(&s) = caps.get(i) {
                    dst.extend(s.as_bytes());
                }
            },
            |name| -> Option<usize> {
                name_to_index
                    .binary_search_by_key(&name, |x| x.0)
                    .ok()
                    .map(|i| name_to_index[i].1)
            },
            &mut dst,
        );
        String::from_utf8(dst).unwrap()
    }

    macro_rules! interp {
        ($name:ident, $map:expr, $caps:expr, $hay:expr, $expected:expr $(,)*) => {
            #[test]
            fn $name() {
                assert_eq!($expected, interpolate_string($map, $caps, $hay));
            }
        };
    }

    interp!(
        interp1,
        vec![("foo", 2)],
        vec!["", "", "xxx"],
        "test $foo test",
        "test xxx test",
    );

    interp!(
        interp2,
        vec![("foo", 2)],
        vec!["", "", "xxx"],
        "test$footest",
        "test",
    );

    interp!(
        interp3,
        vec![("foo", 2)],
        vec!["", "", "xxx"],
        "test${foo}test",
        "testxxxtest",
    );

    interp!(
        interp4,
        vec![("foo", 2)],
        vec!["", "", "xxx"],
        "test$2test",
        "test",
    );

    interp!(
        interp5,
        vec![("foo", 2)],
        vec!["", "", "xxx"],
        "test${2}test",
        "testxxxtest",
    );

    interp!(
        interp6,
        vec![("foo", 2)],
        vec!["", "", "xxx"],
        "test $$foo test",
        "test $foo test",
    );

    interp!(
        interp7,
        vec![("foo", 2)],
        vec!["", "", "xxx"],
        "test $foo",
        "test xxx",
    );

    interp!(
        interp8,
        vec![("foo", 2)],
        vec!["", "", "xxx"],
        "$foo test",
        "xxx test",
    );

    interp!(
        interp9,
        vec![("bar", 1), ("foo", 2)],
        vec!["", "yyy", "xxx"],
        "test $bar$foo",
        "test yyyxxx",
    );

    interp!(
        interp10,
        vec![("bar", 1), ("foo", 2)],
        vec!["", "yyy", "xxx"],
        "test $ test",
        "test $ test",
    );

    interp!(
        interp11,
        vec![("bar", 1), ("foo", 2)],
        vec!["", "yyy", "xxx"],
        "test ${} test",
        "test ${} test",
    );

    interp!(
        interp12,
        vec![("bar", 1), ("foo", 2)],
        vec!["", "yyy", "xxx"],
        "test ${ } test",
        "test ${ } test",
    );

    interp!(
        interp13,
        vec![("bar", 1), ("foo", 2)],
        vec!["", "yyy", "xxx"],
        "test ${a b} test",
        "test ${a b} test",
    );

    interp!(
        interp14,
        vec![("bar", 1), ("foo", 2)],
        vec!["", "yyy", "xxx"],
        "test ${a} test",
        "test  test",
    );
}
