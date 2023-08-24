use std::fs::File;
use std::path::Path;

use memmap::Mmap;

/// 控制内存映射使用的策略。
///
/// 如果在可以使用内存映射的情况下调用搜索器，并且启用了内存映射，
/// 则搜索器将尝试使用内存映射，如果它认为这样做会加快搜索速度。
///
/// 默认情况下，内存映射是禁用的。
#[derive(Clone, Debug)]
pub struct MmapChoice(MmapChoiceImpl);

#[derive(Clone, Debug)]
enum MmapChoiceImpl {
    Auto,
    Never,
}

impl Default for MmapChoice {
    fn default() -> MmapChoice {
        MmapChoice(MmapChoiceImpl::Never)
    }
}

impl MmapChoice {
    /// 在认为有利的情况下使用内存映射。
    ///
    /// 用于确定是否使用内存映射的启发式方法可能依赖于许多因素，
    /// 包括但不限于文件大小和平台。
    ///
    /// 如果特定输入不支持内存映射，或者无法使用内存映射，
    /// 则会改为使用正常的操作系统读取调用。
    ///
    /// # 安全性
    ///
    /// 这个构造函数是不安全的，因为没有明显的方法来在所有平台上
    /// 封装文件支持的内存映射的安全性，同时不会抵消部分或全部它们的好处。
    ///
    /// 调用者需要保证底层文件不会被修改，
    /// 这在许多环境中是不可行的。然而，命令行工具仍然可以决定
    /// 承担读取内存映射时发生 `SIGBUS` 等风险。
    pub unsafe fn auto() -> MmapChoice {
        MmapChoice(MmapChoiceImpl::Auto)
    }

    /// 永不使用内存映射，无论何时。这是默认设置。
    pub fn never() -> MmapChoice {
        MmapChoice(MmapChoiceImpl::Never)
    }

    /// 如果启用了内存映射，并且从给定文件创建内存映射成功，
    /// 并且认为内存映射有利于性能，那么返回一个内存映射。
    ///
    /// 如果尝试打开内存映射失败，则返回 `None`，
    /// 并在调试级别上记录相应的错误（以及文件路径，如果存在）。
    pub(crate) fn open(
        &self,
        file: &File,
        path: Option<&Path>,
    ) -> Option<Mmap> {
        if !self.is_enabled() {
            return None;
        }
        if cfg!(target_os = "macos") {
            // 我们认为 macOS 上的内存映射不是很好用，需要重新评估。
            return None;
        }
        // 安全性：这是可以接受的，因为只有当调用者调用了 `auto` 构造函数时，
        // `MmapChoiceImpl` 才会为 `Auto`，而这本身是不安全的。
        // 因此，这是调用者断言使用内存映射是安全的传播。
        match unsafe { Mmap::map(file) } {
            Ok(mmap) => Some(mmap),
            Err(err) => {
                if let Some(path) = path {
                    log::debug!(
                        "{}: 打开内存映射失败: {}",
                        path.display(),
                        err
                    );
                } else {
                    log::debug!("打开内存映射失败: {}", err);
                }
                None
            }
        }
    }

    /// 是否启用了使用内存映射的策略。
    pub(crate) fn is_enabled(&self) -> bool {
        match self.0 {
            MmapChoiceImpl::Auto => true,
            MmapChoiceImpl::Never => false,
        }
    }
}
