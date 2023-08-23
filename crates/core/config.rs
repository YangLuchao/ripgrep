// 该模块提供了读取 ripgrep 配置文件 "rc" 的功能。这些例程的主要输出是一系列参数，其中每个参数精确地对应一个 shell 参数。

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use bstr::{io::BufReadExt, ByteSlice};
use log;

use crate::Result;

/// 从 ripgrep 配置文件派生一系列参数。
pub fn args() -> Vec<OsString> {
    let config_path = match env::var_os("RIPGREP_CONFIG_PATH") {
        None => return vec![],
        Some(config_path) => {
            if config_path.is_empty() {
                return vec![];
            }
            PathBuf::from(config_path)
        }
    };
    let (args, errs) = match parse(&config_path) {
        Ok((args, errs)) => (args, errs),
        Err(err) => {
            message!(
                "无法读取 RIPGREP_CONFIG_PATH 中指定的文件：{}",
                err
            );
            return vec![];
        }
    };
    if !errs.is_empty() {
        for err in errs {
            message!("{}:{}", config_path.display(), err);
        }
    }
    log::debug!(
        "{}: 从配置文件加载的参数: {:?}",
        config_path.display(),
        args
    );
    args
}

/// 从给定路径解析单个 ripgrep 配置文件。
///
/// 成功时，返回一组 shell 参数，按顺序添加到 ripgrep 命令行的参数之前。
///
/// 如果无法读取文件，则返回错误。如果在解析文件中的一个或多个行时出现问题，则会为每行返回错误，除了成功解析的参数之外。
fn parse<P: AsRef<Path>>(
    path: P,
) -> Result<(Vec<OsString>, Vec<Box<dyn Error>>)> {
    let path = path.as_ref();
    match File::open(&path) {
        Ok(file) => parse_reader(file),
        Err(err) => Err(From::from(format!("{}: {}", path.display(), err))),
    }
}

/// 从给定的读取器解析单个 ripgrep 配置文件。
///
/// 调用者不应提供缓冲读取器，因为此例程将在内部使用其自己的缓冲区。
///
/// 成功时，返回一组 shell 参数，按顺序添加到 ripgrep 命令行的参数之前。
///
/// 如果无法读取读取器，则返回错误。如果在解析一个或多个行时出现问题，则为每行返回错误，除了成功解析的参数之外。
fn parse_reader<R: io::Read>(
    rdr: R,
) -> Result<(Vec<OsString>, Vec<Box<dyn Error>>)> {
    let mut bufrdr = io::BufReader::new(rdr);
    let (mut args, mut errs) = (vec![], vec![]);
    let mut line_number = 0;
    bufrdr.for_byte_line_with_terminator(|line| {
        line_number += 1;

        let line = line.trim();
        if line.is_empty() || line[0] == b'#' {
            return Ok(true);
        }
        match line.to_os_str() {
            Ok(osstr) => {
                args.push(osstr.to_os_string());
            }
            Err(err) => {
                errs.push(format!("{}: {}", line_number, err).into());
            }
        }
        Ok(true)
    })?;
    Ok((args, errs))
}

#[cfg(test)]
mod tests {
    use super::parse_reader;
    use std::ffi::OsString;

    #[test]
    fn basic() {
        let (args, errs) = parse_reader(
            &b"\
# 测试
--context=0
   --smart-case
-u


   # --bar
--foo
"[..],
        )
        .unwrap();
        assert!(errs.is_empty());
        let args: Vec<String> =
            args.into_iter().map(|s| s.into_string().unwrap()).collect();
        assert_eq!(args, vec!["--context=0", "--smart-case", "-u", "--foo",]);
    }

    // 我们测试能否处理在类 Unix 系统上的无效的 UTF-8。
    #[test]
    #[cfg(unix)]
    fn error() {
        use std::os::unix::ffi::OsStringExt;

        let (args, errs) = parse_reader(
            &b"\
quux
foo\xFFbar
baz
"[..],
        )
        .unwrap();
        assert!(errs.is_empty());
        assert_eq!(
            args,
            vec![
                OsString::from("quux"),
                OsString::from_vec(b"foo\xFFbar".to_vec()),
                OsString::from("baz"),
            ]
        );
    }

    // ... 但测试无效的 UTF-8 在 Windows 上失败。
    #[test]
    #[cfg(not(unix))]
    fn error() {
        let (args, errs) = parse_reader(
            &b"\
quux
foo\xFFbar
baz
"[..],
        )
        .unwrap();
        assert_eq!(errs.len(), 1);
        assert_eq!(args, vec![OsString::from("quux"), OsString::from("baz"),]);
    }
}
