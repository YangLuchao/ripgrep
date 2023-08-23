use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::process;

use clap::Shell;

use app::{RGArg, RGArgKind};

#[allow(dead_code)]
#[path = "crates/core/app.rs"]
mod app;

fn main() {
    // OUT_DIR 是由 Cargo 设置的，用于写入任何额外的构建产物。
    let outdir = match env::var_os("OUT_DIR") {
        Some(outdir) => outdir,
        None => {
            eprintln!(
                "OUT_DIR 环境变量未定义。请提交错误报告: \
                 https://github.com/BurntSushi/ripgrep/issues/new"
            );
            process::exit(1);
        }
    };
    fs::create_dir_all(&outdir).unwrap();

    let stamp_path = Path::new(&outdir).join("ripgrep-stamp");
    if let Err(err) = File::create(&stamp_path) {
        panic!("无法写入 {}: {}", stamp_path.display(), err);
    }
    if let Err(err) = generate_man_page(&outdir) {
        eprintln!("生成 man 手册失败: {}", err);
    }

    // 使用 clap 构建自动完成文件。
    let mut app = app::app();
    app.gen_completions("rg", Shell::Bash, &outdir);
    app.gen_completions("rg", Shell::Fish, &outdir);
    app.gen_completions("rg", Shell::PowerShell, &outdir);
    // 注意我们不使用 clap 对 zsh 的支持。相反，zsh 的自动完成在 `complete/_rg` 中手动维护。

    // 将当前的 Git 哈希值用于构建。
    if let Some(rev) = git_revision_hash() {
        println!("cargo:rustc-env=RIPGREP_BUILD_GIT_HASH={}", rev);
    }
    // 嵌入 Windows 执行文件的清单并设置一些链接器选项。这主要是为了在 Windows 上启用长路径支持。
    // 我相信这仍然需要在注册表中启用长路径支持。但如果启用了，这将允许 ripgrep 使用超过 260 字符的 C:\... 样式路径。
    set_windows_exe_options();
}

fn set_windows_exe_options() {
    static MANIFEST: &str = "pkg/windows/Manifest.xml";

    let Ok(target_os) = env::var("CARGO_CFG_TARGET_OS") else { return };
    let Ok(target_env) = env::var("CARGO_CFG_TARGET_ENV") else { return };
    if !(target_os == "windows" && target_env == "msvc") {
        return;
    }

    let Ok(mut manifest) = env::current_dir() else { return };
    manifest.push(MANIFEST);
    let Some(manifest) = manifest.to_str() else { return };

    println!("cargo:rerun-if-changed={}", MANIFEST);
    // 嵌入 Windows 应用程序清单文件。
    println!("cargo:rustc-link-arg-bin=rg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bin=rg=/MANIFESTINPUT:{manifest}");
    // 将链接器警告转换为错误。这有助于调试，否则警告会被压制（我相信）。
    println!("cargo:rustc-link-arg-bin=rg=/WX");
}

fn git_revision_hash() -> Option<String> {
    let result = process::Command::new("git")
        .args(&["rev-parse", "--short=10", "HEAD"])
        .output();
    result.ok().and_then(|output| {
        let v = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    })
}

fn generate_man_page<P: AsRef<Path>>(outdir: P) -> io::Result<()> {
    // 如果未安装 asciidoctor，则回退到 asciidoc。
    if let Err(err) = process::Command::new("asciidoctor").output() {
        eprintln!("无法运行 'asciidoctor' 二进制文件，回退到 'a2x'。");
        eprintln!("运行 'asciidoctor' 时出错: {}", err);
        return legacy_generate_man_page::<P>(outdir);
    }
    // 1. 读取 asciidoctor 模板。
    // 2. 使用自动生成的文档对模板进行插值。
    // 3. 将插值保存到磁盘。
    // 4. 使用 asciidoctor 转换为 man 手册。
    let outdir = outdir.as_ref();
    let cwd = env::current_dir()?;
    let tpl_path = cwd.join("doc").join("rg.1.txt.tpl");
    let txt_path = outdir.join("rg.1.txt");

    let mut tpl = String::new();
    File::open(&tpl_path)?.read_to_string(&mut tpl)?;
    let options =
        formatted_options()?.replace("&#123;", "{").replace("&#125;", "}");
    tpl = tpl.replace("{OPTIONS}", &options);

    let githash = git_revision_hash();
    let githash = githash.as_ref().map(|x| &**x);
    tpl = tpl.replace("{VERSION}", &app::long_version(githash, false));

    File::create(&txt_path)?.write_all(tpl.as_bytes())?;
    let result = process::Command::new("asciidoctor")
        .arg("--doctype")
        .arg("manpage")
        .arg("--backend")
        .arg("manpage")
        .arg(&txt_path)
        .spawn()?
        .wait()?;
    if !result.success() {
        let msg = format!("'asciidoctor' 失败，退出码: {:?}", result.code());
        return Err(ioerr(msg));
    }
    Ok(())
}

fn legacy_generate_man_page<P: AsRef<Path>>(outdir: P) -> io::Result<()> {
    // 如果未安装 asciidoc，则不执行任何操作。
    if let Err(err) = process::Command::new("a2x").output() {
        eprintln!("无法运行 'a2x' 二进制文件，跳过手册生成。");
        eprintln!("运行 'a2x' 时出错: {}", err);
        return Ok(());
    }
    // 1. 读取 asciidoc 模板。
    // 2. 使用自动生成的文档对模板进行插值。
    // 3. 将插值保存到磁盘。
    // 4. 使用 a2x（asciidoc 的一部分）转换为 man 手册。
    let outdir = outdir.as_ref();
    let cwd = env::current_dir()?;
    let tpl_path = cwd.join("doc").join("rg.1.txt.tpl");
    let txt_path = outdir.join("rg.1.txt");

    let mut tpl = String::new();
    File::open(&tpl_path)?.read_to_string(&mut tpl)?;
    tpl = tpl.replace("{OPTIONS}", &formatted_options()?);

    let githash = git_revision_hash();
    let githash = githash.as_ref().map(|x| &**x);
    tpl = tpl.replace("{VERSION}", &app::long_version(githash, false));

    File::create(&txt_path)?.write_all(tpl.as_bytes())?;
    let result = process::Command::new("a2x")
        .arg("--no-xmllint")
        .arg("--doctype")
        .arg("manpage")
        .arg("--format")
        .arg("manpage")
        .arg(&txt_path)
        .spawn()?
        .wait()?;
    if !result.success() {
        let msg = format!("'a2x' 失败，退出码: {:?}", result.code());
        return Err(ioerr(msg));
    }
    Ok(())
}

fn formatted_options() -> io::Result<String> {
    let mut args = app::all_args_and_flags();
    args.sort_by(|x1, x2| x1.name.cmp(&x2.name));

    let mut formatted = vec![];
    for arg in args {
        if arg.hidden {
            continue;
        }
        // ripgrep 只有两个定位参数，而且可能永远只有两个定位参数，所以我们直接在模板中硬编码它们。
        if let app::RGArgKind::Positional { .. } = arg.kind {
            continue;
        }
        formatted.push(formatted_arg(&arg)?);
    }
    Ok(formatted.join("\n\n"))
}

fn formatted_arg(arg: &RGArg) -> io::Result<String> {
    match arg.kind {
        RGArgKind::Positional { .. } => {
            panic!("意外的定位参数")
        }
        RGArgKind::Switch { long, short, multiple } => {
            let mut out = vec![];

            let mut header = format!("--{}", long);
            if let Some(short) = short {
                header = format!("-{}, {}", short, header);
            }
            if multiple {
                header = format!("*{}* ...::", header);
            } else {
                header = format!("*{}*::", header);
            }
            writeln!(out, "{}", header)?;
            writeln!(out, "{}", formatted_doc_txt(arg)?)?;

            Ok(String::from_utf8(out).unwrap())
        }
        RGArgKind::Flag { long, short, value_name, multiple, .. } => {
            let mut out = vec![];

            let mut header = format!("--{}", long);
            if let Some(short) = short {
                header = format!("-{}, {}", short, header);
            }
            if multiple {
                header = format!("*{}* _{}_ ...::", header, value_name);
            } else {
                header = format!("*{}* _{}_::", header, value_name);
            }
            writeln!(out, "{}", header)?;
            writeln!(out, "{}", formatted_doc_txt(arg)?)?;

            Ok(String::from_utf8(out).unwrap())
        }
    }
}

fn formatted_doc_txt(arg: &RGArg) -> io::Result<String> {
    let paragraphs: Vec<String> = arg
        .doc_long
        .replace("{", "&#123;")
        .replace("}", r"&#125;")
        // 以正确的方式在 man 手册中呈现 **。我们不能直接将这些疯狂的 +++ 放入帮助文本中，因为这会在 --help 输出中直接显示。
        .replace("*-g 'foo/**'*", "*-g +++'foo/**'+++*")
        .split("\n\n")
        .map(|s| s.to_string())
        .collect();
    if paragraphs.is_empty() {
        return Err(ioerr(format!("缺少 --{} 的文档", arg.name)));
    }
    let first = format!("  {}", paragraphs[0].replace("\n", "\n  "));
    if paragraphs.len() == 1 {
        return Ok(first);
    }
    Ok(format!("{}\n+\n{}", first, paragraphs[1..].join("\n+\n")))
}

fn ioerr(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
}
