use std::path::Path;

use ignore::{self, DirEntry};
use log;

/// 描述如何构建主题的配置。
#[derive(Clone, Debug)]
struct Config {
    strip_dot_prefix: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config { strip_dot_prefix: false }
    }
}

/// 用于构建要搜索的内容的构建器。
#[derive(Clone, Debug)]
pub struct SubjectBuilder {
    config: Config,
}

impl SubjectBuilder {
    /// 返回一个具有默认配置的新主题构建器。
    pub fn new() -> SubjectBuilder {
        SubjectBuilder { config: Config::default() }
    }

    /// 根据可能缺少的目录条目创建一个新主题。
    ///
    /// 如果目录条目不存在，则相应的错误将在配置了消息的情况下记录。
    /// 否则，如果判断主题是可搜索的，则返回它。
    pub fn build_from_result(
        &self,
        result: Result<DirEntry, ignore::Error>,
    ) -> Option<Subject> {
        match result {
            Ok(dent) => self.build(dent),
            Err(err) => {
                err_message!("{}", err);
                None
            }
        }
    }

    /// 使用此构建器的配置创建一个新主题。
    ///
    /// 如果无法创建主题或者不应该进行搜索，那么在发出任何相关日志消息后，它将返回 `None`。
    pub fn build(&self, dent: DirEntry) -> Option<Subject> {
        let subj =
            Subject { dent, strip_dot_prefix: self.config.strip_dot_prefix };
        if let Some(ignore_err) = subj.dent.error() {
            ignore_message!("{}", ignore_err);
        }
        // 如果此条目是由最终用户明确提供的，则始终要搜索它。
        if subj.is_explicit() {
            return Some(subj);
        }
        // 此时，只有在它明确是文件时才想要搜索它。
        // 这排除了符号链接。 （如果 ripgrep 配置为跟随符号链接，那么它们已经被目录遍历时跟随了。）
        if subj.is_file() {
            return Some(subj);
        }
        // 我们什么都没有。发出调试消息，但仅当这不是目录时。否则，为目录发出消息只会产生噪音。
        if !subj.is_dir() {
            log::debug!(
                "忽略 {}: 未能通过主题过滤器：文件类型: {:?}, 元数据: {:?}",
                subj.dent.path().display(),
                subj.dent.file_type(),
                subj.dent.metadata()
            );
        }
        None
    }

    /// 当启用时，如果主题的文件路径以 `./` 开头，则将其去除。
    ///
    /// 当隐式搜索当前工作目录时，这很有用。
    pub fn strip_dot_prefix(&mut self, yes: bool) -> &mut SubjectBuilder {
        self.config.strip_dot_prefix = yes;
        self
    }
}

/// 主题是我们要搜索的内容。通常情况下，主题是文件或标准输入。
#[derive(Clone, Debug)]
pub struct Subject {
    dent: DirEntry,
    strip_dot_prefix: bool,
}

impl Subject {
    /// 返回与此主题对应的文件路径。
    ///
    /// 如果此主题对应于标准输入，则返回特殊的 `<stdin>` 路径。
    pub fn path(&self) -> &Path {
        if self.strip_dot_prefix && self.dent.path().starts_with("./") {
            self.dent.path().strip_prefix("./").unwrap()
        } else {
            self.dent.path()
        }
    }

    /// 当且仅当此条目对应于标准输入时返回 true。
    pub fn is_stdin(&self) -> bool {
        self.dent.is_stdin()
    }

    /// 当且仅当此条目对应于由最终用户明确提供的要搜索的主题时返回 true。
    ///
    /// 通常，这对应于标准输入或显式的文件路径参数。
    /// 例如，在 `rg foo some-file ./some-dir/` 中，`some-file` 是一个显式主题，
    /// 但是在 `./some-dir/some-other-file` 中则不是。
    ///
    /// 但要注意，ripgrep 不会看透 shell 的通配符扩展。
    /// 例如，在 `rg foo ./some-dir/*` 中，`./some-dir/some-other-file` 将被视为显式主题。
    pub fn is_explicit(&self) -> bool {
        // 标准输入很明显。
        // 当条目的深度为 0 时，表示它是我们的目录迭代器明确提供的，
        // 这意味着它最终是由最终用户明确提供的。!is_dir 检查意味着我们希望即使是符号链接的文件也要进行搜索，
        // 这是因为它们是明确提供的。（我们永远不希望尝试搜索目录。）
        self.is_stdin() || (self.dent.depth() == 0 && !self.is_dir())
    }

    /// 当且仅当此主题在跟随符号链接后指向目录时返回 true。
    fn is_dir(&self) -> bool {
        let ft: std::fs::FileType = match self.dent.file_type() {
            None => return false,
            Some(ft) => ft,
        };
        if ft.is_dir() {
            return true;
        }
        // 如果这是一个符号链接，我们想要跟随它以确定它是否是目录。
        self.dent.path_is_symlink() && self.dent.path().is_dir()
    }

    /// 当且仅当此主题指向文件时返回 true。
    fn is_file(&self) -> bool {
        self.dent.file_type().map_or(false, |ft| ft.is_file())
    }
}
