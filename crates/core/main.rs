use std::error;
use std::io::{self, Write};
use std::process;
use std::sync::Mutex;
use std::time::Instant;

use ignore::WalkState;

use args::Args;
use subject::Subject;

#[macro_use]
mod messages;

mod app;
mod args;
mod config;
mod logger;
mod path_printer;
mod search;
mod subject;

// 由于 Rust 不再默认使用 jemalloc，ripgrep 将默认使用系统分配器。
// 在 Linux 上，通常会是 glibc 的分配器，它相当不错。特别是，ripgrep 的工作负载并不是特别重的分配工作，所以对于 ripgrep 来说，glibc 的分配器和 jemalloc 之间的差异实际上不是很大。

// 然而，当使用 musl 构建 ripgrep 时，这意味着 ripgrep 将使用 musl 的分配器，而 musl 的分配器似乎要差很多。（musl 的目标不是拥有最快的所有版本。它的目标是小而易于静态编译。）尽管 ripgrep 并不特别需要大量的分配，但 musl 的分配器似乎会严重减慢 ripgrep 的速度。因此，在使用 musl 构建时，我们使用 jemalloc。

// 我们不会无条件地使用 jemalloc，因为默认情况下使用系统的默认分配器可能更好。此外，jemalloc 似乎会增加编译时间。

// 此外，我们仅在 64 位系统上执行此操作，因为 jemalloc 不支持 i686。
#[cfg(all(target_env = "musl", target_pointer_width = "64"))]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

type Result<T> = ::std::result::Result<T, Box<dyn error::Error>>;

fn main() {
    if let Err(err) = Args::parse().and_then(try_main) {
        eprintln_locked!("{}", err);
        process::exit(2);
    }
}

fn try_main(args: Args) -> Result<()> {
    use args::Command::*;

    let matched: bool = match args.command() {
        Search => search(&args),
        SearchParallel => search_parallel(&args),
        SearchNever => Ok(false),
        Files => files(&args),
        FilesParallel => files_parallel(&args),
        Types => types(&args),
        PCRE2Version => pcre2_version(&args),
    }?;
    if matched && (args.quiet() || !messages::errored()) {
        process::exit(0)
    } else if messages::errored() {
        process::exit(2)
    } else {
        process::exit(1)
    }
}

/// 单线程搜索的顶级入口点。这会递归地遍历文件列表（默认为当前目录），并依次搜索每个文件。
fn search(args: &Args) -> Result<bool> {
    /// 这个函数是核心部分，它允许我们在每个文件上调用相同的迭代代码，无论是按照底层目录遍历产生的文件流式处理，
    /// 还是在进行收集和排序之后的情况。
    fn iter(
        args: &Args,
        subjects: impl Iterator<Item = Subject>,
        started_at: std::time::Instant,
    ) -> Result<bool> {
        let quit_after_match: bool = args.quit_after_match()?;
        let mut stats: Option<grep::printer::Stats> = args.stats()?;
        let mut searcher: search::SearchWorker<grep::cli::StandardStream> =
            args.search_worker(args.stdout())?;
        let mut matched: bool = false;
        let mut searched: bool = false;

        for subject in subjects {
            searched = true;
            let search_result: search::SearchResult = match searcher
                .search(&subject)
            {
                Ok(search_result) => search_result,
                // 破裂的管道意味着优雅终止。
                Err(err) if err.kind() == io::ErrorKind::BrokenPipe => break,
                Err(err) => {
                    err_message!("{}: {}", subject.path().display(), err);
                    continue;
                }
            };
            matched |= search_result.has_match();
            if let Some(ref mut stats) = stats {
                *stats += search_result.stats().unwrap();
            }
            if matched && quit_after_match {
                break;
            }
        }
        if args.using_default_path() && !searched {
            eprint_nothing_searched();
        }
        if let Some(ref stats) = stats {
            let elapsed = Instant::now().duration_since(started_at);
            // 我们不关心是否能成功打印这个。
            let _ = searcher.print_stats(elapsed, stats);
        }
        Ok(matched)
    }

    // 开始时间
    let started_at: Instant = Instant::now();
    // 搜索内容构造器
    let subject_builder: subject::SubjectBuilder = args.subject_builder();
    let subjects = args
        .walker()? // 构造一个执行器
        // 创建一个同时过滤和映射的迭代器。
        .filter_map(
            |result: std::result::Result<ignore::DirEntry, ignore::Error>| {
                // 根据执行期结果构建主题
                subject_builder.build_from_result(result)
            },
        );
    if args.needs_stat_sort() {
        // 将迭代器设置为排序并迭代
        let subjects: std::vec::IntoIter<Subject> =
            args.sort_by_stat(subjects).into_iter();
        iter(args, subjects, started_at)
    } else {
        iter(args, subjects, started_at)
    }
}
/// 多线程搜索的顶级入口点。并行性是通过递归目录遍历实现的。
/// 我们只需要为每个文件提供执行搜索的 worker。
///
/// 请求 ripgrep 排序的输出（例如 `--sort path`）将自动禁用并行处理，因此此处不处理排序。
fn search_parallel(args: &Args) -> Result<bool> {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::SeqCst;

    let quit_after_match = args.quit_after_match()?;
    let started_at = Instant::now();
    let subject_builder = args.subject_builder();
    let bufwtr = args.buffer_writer()?;
    let stats = args.stats()?.map(Mutex::new);
    let matched = AtomicBool::new(false);
    let searched = AtomicBool::new(false);
    let mut searcher_err = None;
    args.walker_parallel()?.run(|| {
        let bufwtr = &bufwtr;
        let stats = &stats;
        let matched = &matched;
        let searched = &searched;
        let subject_builder = &subject_builder;
        let mut searcher = match args.search_worker(bufwtr.buffer()) {
            Ok(searcher) => searcher,
            Err(err) => {
                searcher_err = Some(err);
                return Box::new(move |_| WalkState::Quit);
            }
        };

        Box::new(move |result| {
            let subject = match subject_builder.build_from_result(result) {
                Some(subject) => subject,
                None => return WalkState::Continue,
            };
            searched.store(true, SeqCst);
            searcher.printer().get_mut().clear();
            let search_result = match searcher.search(&subject) {
                Ok(search_result) => search_result,
                Err(err) => {
                    err_message!("{}: {}", subject.path().display(), err);
                    return WalkState::Continue;
                }
            };
            if search_result.has_match() {
                matched.store(true, SeqCst);
            }
            if let Some(ref locked_stats) = *stats {
                let mut stats = locked_stats.lock().unwrap();
                *stats += search_result.stats().unwrap();
            }
            if let Err(err) = bufwtr.print(searcher.printer().get_mut()) {
                // Broken pipe意味着优雅终止。
                if err.kind() == io::ErrorKind::BrokenPipe {
                    return WalkState::Quit;
                }
                // 否则，我们继续进行。
                err_message!("{}: {}", subject.path().display(), err);
            }
            if matched.load(SeqCst) && quit_after_match {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });
    if let Some(err) = searcher_err.take() {
        return Err(err);
    }
    if args.using_default_path() && !searched.load(SeqCst) {
        eprint_nothing_searched();
    }
    if let Some(ref locked_stats) = stats {
        let elapsed = Instant::now().duration_since(started_at);
        let stats = locked_stats.lock().unwrap();
        let mut searcher = args.search_worker(args.stdout())?;
        // 对于打印可能失败的情况，我们不关心。
        let _ = searcher.print_stats(elapsed, &stats);
    }
    Ok(matched.load(SeqCst))
}

fn eprint_nothing_searched() {
    err_message!(
        "未搜索到任何文件，这意味着 ripgrep 可能应用了您未预期的过滤器。\n\
         使用 --debug 标志将显示为何跳过文件。"
    );
}

/// 无搜索的情况下列出文件的顶级入口点。这会递归地遍历文件列表（默认为当前目录），
/// 并使用单个线程顺序地打印每个路径。
fn files(args: &Args) -> Result<bool> {
    /// 例程的核心在这里。这使我们可以调用相同的迭代代码，而不管是以底层目录遍历产生的文件流形式
    /// 还是已经收集并排序（例如）的文件。
    fn iter(
        args: &Args,
        subjects: impl Iterator<Item = Subject>,
    ) -> Result<bool> {
        let quit_after_match = args.quit_after_match()?;
        let mut matched = false;
        let mut path_printer = args.path_printer(args.stdout())?;

        for subject in subjects {
            matched = true;
            if quit_after_match {
                break;
            }
            if let Err(err) = path_printer.write_path(subject.path()) {
                // Broken pipe意味着优雅终止。
                if err.kind() == io::ErrorKind::BrokenPipe {
                    break;
                }
                // 否则，我们有一些阻止我们写入stdout的其他错误，因此我们应该将其上升。
                return Err(err.into());
            }
        }
        Ok(matched)
    }

    let subject_builder = args.subject_builder();
    let subjects = args
        .walker()?
        .filter_map(|result| subject_builder.build_from_result(result));
    if args.needs_stat_sort() {
        let subjects = args.sort_by_stat(subjects).into_iter();
        iter(args, subjects)
    } else {
        iter(args, subjects)
    }
}

/// 无搜索的情况下并行列出文件的顶级入口点。这会递归地遍历文件列表（默认为当前目录），
/// 并使用多个线程顺序地打印每个路径。
///
/// 请求 ripgrep 排序的输出（例如 `--sort path`）将自动禁用并行处理，因此此处不处理排序。
fn files_parallel(args: &Args) -> Result<bool> {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::mpsc;
    use std::thread;

    let quit_after_match = args.quit_after_match()?;
    let subject_builder = args.subject_builder();
    let mut path_printer = args.path_printer(args.stdout())?;
    let matched = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<Subject>();

    let print_thread = thread::spawn(move || -> io::Result<()> {
        for subject in rx.iter() {
            path_printer.write_path(subject.path())?;
        }
        Ok(())
    });
    args.walker_parallel()?.run(|| {
        let subject_builder = &subject_builder;
        let matched = &matched;
        let tx = tx.clone();

        Box::new(move |result| {
            let subject = match subject_builder.build_from_result(result) {
                Some(subject) => subject,
                None => return WalkState::Continue,
            };
            matched.store(true, SeqCst);
            if quit_after_match {
                WalkState::Quit
            } else {
                match tx.send(subject) {
                    Ok(_) => WalkState::Continue,
                    Err(_) => WalkState::Quit,
                }
            }
        })
    });
    drop(tx);
    if let Err(err) = print_thread.join().unwrap() {
        // Broken pipe意味着优雅终止，所以继续执行。
        // 否则，写入stdout时发生了一些问题，因此上升。
        if err.kind() != io::ErrorKind::BrokenPipe {
            return Err(err.into());
        }
    }
    Ok(matched.load(SeqCst))
}

/// --type-list 的顶级入口点。
fn types(args: &Args) -> Result<bool> {
    let mut count = 0;
    let mut stdout = args.stdout();
    for def in args.type_defs()? {
        count += 1;
        stdout.write_all(def.name().as_bytes())?;
        stdout.write_all(b": ")?;

        let mut first = true;
        for glob in def.globs() {
            if !first {
                stdout.write_all(b", ")?;
            }
            stdout.write_all(glob.as_bytes())?;
            first = false;
        }
        stdout.write_all(b"\n")?;
    }
    Ok(count > 0)
}

/// --pcre2-version 的顶级入口点。
fn pcre2_version(args: &Args) -> Result<bool> {
    #[cfg(feature = "pcre2")]
    fn imp(args: &Args) -> Result<bool> {
        use grep::pcre2;

        let mut stdout = args.stdout();

        let (major, minor) = pcre2::version();
        writeln!(stdout, "PCRE2 {}.{} is available", major, minor)?;

        if cfg!(target_pointer_width = "64") && pcre2::is_jit_available() {
            writeln!(stdout, "JIT is available")?;
        }
        Ok(true)
    }

    #[cfg(not(feature = "pcre2"))]
    fn imp(args: &Args) -> Result<bool> {
        let mut stdout = args.stdout();
        writeln!(stdout, "此版本的 ripgrep 中不可用 PCRE2.")?;
        Ok(false)
    }

    imp(args)
}
