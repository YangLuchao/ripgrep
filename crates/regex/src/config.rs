use {
    grep_matcher::{ByteSet, LineTerminator},
    regex_automata::meta::Regex,
    regex_syntax::{
        ast,
        hir::{self, Hir, HirKind},
    },
};

use crate::{
    ast::AstAnalysis, error::Error, non_matching::non_matching_bytes,
    strip::strip_from_match,
};
/// `Config`表示此crate中正则表达式匹配器的配置。
/// 此配置本身是`regex` crate中的各种设置选项的组合，同时还包括其他`grep-matcher`特定的选项。
///
/// 可以使用配置来构建一个“配置过的”HIR表达式。配置过的HIR表达式是一个HIR表达式，
/// 它知道生成它的配置，并对该HIR提供转换，以保留配置。
#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) case_insensitive: bool,
    pub(crate) case_smart: bool,
    pub(crate) multi_line: bool,
    pub(crate) dot_matches_new_line: bool,
    pub(crate) swap_greed: bool,
    pub(crate) ignore_whitespace: bool,
    pub(crate) unicode: bool,
    pub(crate) octal: bool,
    pub(crate) size_limit: usize,
    pub(crate) dfa_size_limit: usize,
    pub(crate) nest_limit: u32,
    pub(crate) line_terminator: Option<LineTerminator>,
    pub(crate) crlf: bool,
    pub(crate) word: bool,
    pub(crate) fixed_strings: bool,
    pub(crate) whole_line: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            case_insensitive: false,
            case_smart: false,
            multi_line: false,
            dot_matches_new_line: false,
            swap_greed: false,
            ignore_whitespace: false,
            unicode: true,
            octal: false,
            // 这些大小限制要比默认情况下的`regex` crate中的大得多。
            size_limit: 100 * (1 << 20),
            dfa_size_limit: 1000 * (1 << 20),
            nest_limit: 250,
            line_terminator: None,
            crlf: false,
            word: false,
            fixed_strings: false,
            whole_line: false,
        }
    }
}

impl Config {
    /// 使用此配置从给定的模式构建一个HIR。返回的HIR对应于给定模式的交替。
    pub(crate) fn build_many<P: AsRef<str>>(
        &self,
        patterns: &[P],
    ) -> Result<ConfiguredHIR, Error> {
        ConfiguredHIR::new(self.clone(), patterns)
    }

    /// 根据“smart_case”配置选项，返回true当且仅当应该对此模式进行不区分大小写的匹配。
    fn is_case_insensitive(&self, analysis: &AstAnalysis) -> bool {
        if self.case_insensitive {
            return true;
        }
        if !self.case_smart {
            return false;
        }
        analysis.any_literal() && !analysis.any_uppercase()
    }

    /// 返回是否应将给定的模式视为“固定字符串”字面值。
    /// 这与仅查询`fixed_strings`选项不同，因为如果选项为false，
    /// 则在某些情况下，如果模式本身与字面值不可区分，则此方法仍将返回true。
    ///
    /// 主要思想在于，如果返回true，则可以安全地从给定的模式构建`regex_syntax::hir::Hir`值，
    /// 该值是固定字符串字面值的交替。
    fn is_fixed_strings<P: AsRef<str>>(&self, patterns: &[P]) -> bool {
        // 当这些选项启用时，我们确实需要解析模式并让它们通过标准的HIR转换过程，
        // 以便应用大小写折叠转换。
        if self.case_insensitive || self.case_smart {
            return false;
        }
        // 即使启用了whole_line或word选项，这两种情况都可以通过用固定字符串字面值的交替包装生成的Hir来实现。
        // 所以至少在这里，我们不关心word或whole_line设置。
        if self.fixed_strings {
            // ...但是，如果任何字面值包含行终止符，那么我们必须中止，因为这最终将导致错误。
            if let Some(lineterm) = self.line_terminator {
                for p in patterns.iter() {
                    if has_line_terminator(lineterm, p.as_ref()) {
                        return false;
                    }
                }
            }
            return true;
        }
        // 在这种情况下，只有当模式不包含元字符时，我们才能手动构建Hir。
        // 如果它们包含元字符，则需要将它们通过标准的解析/转换过程发送。
        for p in patterns.iter() {
            let p = p.as_ref();
            if p.chars().any(regex_syntax::is_meta_character) {
                return false;
            }
            // 与上面的fixed_strings设置类似。如果模式中有任何位置有行终止符，那么我们必须中止并允许错误发生。
            if let Some(lineterm) = self.line_terminator {
                if has_line_terminator(lineterm, p) {
                    return false;
                }
            }
        }
        true
    }
}
/// “配置过的”HIR表达式，该表达式意识到生成此HIR的配置。
///
/// 由于跟踪了配置，因此具有此类型的值可以以保留配置的方式转换为其他HIR表达式（或正则表达式）。
/// 例如，`fast_line_regex`方法将对内部HIR应用字面量提取，并使用它构建一个新的正则表达式，
/// 以与生成此HIR的配置保持一致。例如，配置的HIR上设置的大小限制将传播到随后构造的任何HIR或正则表达式中。
#[derive(Clone, Debug)]
pub(crate) struct ConfiguredHIR {
    config: Config,
    hir: Hir,
}

impl ConfiguredHIR {
    /// 将给定的模式解析为表示给定模式交替的单个HIR表达式。
    fn new<P: AsRef<str>>(
        config: Config,
        patterns: &[P],
    ) -> Result<ConfiguredHIR, Error> {
        let hir = if config.is_fixed_strings(patterns) {
            let mut alts = vec![];
            for p in patterns.iter() {
                alts.push(Hir::literal(p.as_ref().as_bytes()));
            }
            log::debug!("从{}个固定字符串字面量组装HIR", alts.len());
            let hir = Hir::alternation(alts);
            hir
        } else {
            let mut alts = vec![];
            for p in patterns.iter() {
                alts.push(if config.fixed_strings {
                    format!("(?:{})", regex_syntax::escape(p.as_ref()))
                } else {
                    format!("(?:{})", p.as_ref())
                });
            }
            let pattern = alts.join("|");
            let ast = ast::parse::ParserBuilder::new()
                .nest_limit(config.nest_limit)
                .octal(config.octal)
                .ignore_whitespace(config.ignore_whitespace)
                .build()
                .parse(&pattern)
                .map_err(Error::generic)?;
            let analysis = AstAnalysis::from_ast(&ast);
            let mut hir = hir::translate::TranslatorBuilder::new()
                .utf8(false)
                .case_insensitive(config.is_case_insensitive(&analysis))
                .multi_line(config.multi_line)
                .dot_matches_new_line(config.dot_matches_new_line)
                .crlf(config.crlf)
                .swap_greed(config.swap_greed)
                .unicode(config.unicode)
                .build()
                .translate(&pattern, &ast)
                .map_err(Error::generic)?;
            // 我们不需要为上面的fixed-strings情况执行此操作，
            // 因为如果任何模式包含行终止符，is_fixed_strings将返回false。
            // 因此，我们不需要将其剥离。
            //
            // 为了避免在ripgrep为一组巨大的字面值进行搜索时进行此操作，
            // 我们避免在上面的情况下执行此操作。这实际上可能需要一些时间。
            // 它不是很大，但是可以注意到。
            hir = match config.line_terminator {
                None => hir,
                Some(line_term) => strip_from_match(hir, line_term)?,
            };
            hir
        };
        Ok(ConfiguredHIR { config, hir })
    }

    /// 返回对底层配置的引用。
    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    /// 返回对底层HIR的引用。
    pub(crate) fn hir(&self) -> &Hir {
        &self.hir
    }

    /// 将此HIR转换为可用于匹配的正则表达式。
    pub(crate) fn to_regex(&self) -> Result<Regex, Error> {
        let meta = Regex::config()
            .utf8_empty(false)
            .nfa_size_limit(Some(self.config.size_limit))
            // 我们不为此暴露一个选项，因为一遍DFA通常不是ripgrep的性能瓶颈。
            // 但我们给了它比默认值更多的空间。
            .onepass_size_limit(Some(10 * (1 << 20)))
            // 这里同样。默认的完全DFA限制非常小，但是对于ripgrep来说，
            // 我们可以为构建它们花费更多的时间。
            .dfa_size_limit(Some(1 * (1 << 20)))
            .dfa_state_limit(Some(1_000))
            .hybrid_cache_capacity(self.config.dfa_size_limit);
        Regex::builder()
            .configure(meta)
            .build_from_hir(&self.hir)
            .map_err(Error::regex)
    }

    /// 计算此HIR表达式的非匹配字节集。
    pub(crate) fn non_matching_bytes(&self) -> ByteSet {
        non_matching_bytes(&self.hir)
    }

    /// 返回此表达式上配置的行终止符。
    ///
    /// 当我们有起始/结束锚点（而不是行锚点）时，快速行搜索路径不完全正确。
    /// 或者至少，它与慢路径不匹配。换句话说，具有文本锚点时，由于行终止符没有被剥离，
    /// 快速搜索路径可能无法执行。
    ///
    /// 由于在面向行的搜索上很少使用文本锚点（基本上始终启用多行模式），
    /// 我们在存在文本锚点时禁用此优化。我们通过不返回行终止符来禁用它，因为没有行终止符，
    /// 无法执行快速搜索路径。
    ///
    /// 实际上，上面的解释已经不太正确。后来，另一个优化被添加，
    /// 如果行终止符位于绝对不会成为匹配一部分的字节集中，
    /// 那么高级搜索基础架构会认为仍然可以采用更快，更灵活的逐行搜索路径。
    /// 这种优化适用于启用多行搜索（不是多行模式）的情况。在这种情况下，
    /// 没有配置的行终止符，因为允许正则表达式匹配行终止符。
    /// 但是，如果正则表达式保证不会跨多行匹配，尽管请求了多行搜索，
    /// 我们仍然可以执行更快和更灵活的逐行搜索。这就是为什么非匹配提取例程会在存在\A和\z时删除\n，
    /// 即使这不太正确...
    ///
    /// 参见：<https://github.com/BurntSushi/ripgrep/issues/2260>
    pub(crate) fn line_terminator(&self) -> Option<LineTerminator> {
        if self.hir.properties().look_set().contains_anchor_haystack() {
            None
        } else {
            self.config.line_terminator
        }
    }

    /// 将此配置的HIR转换为一个在匹配的两侧都对应于单词边界时才匹配的等效HIR。
    ///
    /// 请注意，返回的HIR类似于将`pat`转换为`(?m:^|\W)(pat)(?m:$|\W)`。
    /// 也就是说，真正的匹配在捕获组`1`中，而不是`0`。
    pub(crate) fn into_word(self) -> Result<ConfiguredHIR, Error> {
        // 理论上构建\W的HIR不应失败，但可能存在一些病态情况
        // （特别是在限制的某些值方面），理论上可能会失败。
        let non_word = {
            let mut config = self.config.clone();
            config.fixed_strings = false;
            ConfiguredHIR::new(config, &[r"\W"])?
        };
        let line_anchor_start = Hir::look(self.line_anchor_start());
        let line_anchor_end = Hir::look(self.line_anchor_end());
        let hir = Hir::concat(vec![
            Hir::alternation(vec![line_anchor_start, non_word.hir.clone()]),
            Hir::capture(hir::Capture {
                index: 1,
                name: None,
                sub: Box::new(renumber_capture_indices(self.hir)?),
            }),
            Hir::alternation(vec![non_word.hir, line_anchor_end]),
        ]);
        Ok(ConfiguredHIR { config: self.config, hir })
    }

    /// 将此配置的HIR转换为等效的HIR，但是只有在行的起始和结束处匹配时才匹配。
    pub(crate) fn into_whole_line(self) -> ConfiguredHIR {
        let line_anchor_start = Hir::look(self.line_anchor_start());
        let line_anchor_end = Hir::look(self.line_anchor_end());
        let hir =
            Hir::concat(vec![line_anchor_start, self.hir, line_anchor_end]);
        ConfiguredHIR { config: self.config, hir }
    }

    /// 将此配置的HIR转换为等效的HIR，但是只有在哈士奇的起始和结束处匹配时才匹配。
    pub(crate) fn into_anchored(self) -> ConfiguredHIR {
        let hir = Hir::concat(vec![
            Hir::look(hir::Look::Start),
            self.hir,
            Hir::look(hir::Look::End),
        ]);
        ConfiguredHIR { config: self.config, hir }
    }

    /// 返回此配置的“起始行”锚点。
    fn line_anchor_start(&self) -> hir::Look {
        if self.config.crlf {
            hir::Look::StartCRLF
        } else {
            hir::Look::StartLF
        }
    }

    /// 返回此配置的“结束行”锚点。
    fn line_anchor_end(&self) -> hir::Look {
        if self.config.crlf {
            hir::Look::EndCRLF
        } else {
            hir::Look::EndLF
        }
    }
}

/// 这会将给定的HIR中的每个捕获组的索引增加1。如果任何增量导致溢出，则返回错误。
fn renumber_capture_indices(hir: Hir) -> Result<Hir, Error> {
    Ok(match hir.into_kind() {
        HirKind::Empty => Hir::empty(),
        HirKind::Literal(hir::Literal(lit)) => Hir::literal(lit),
        HirKind::Class(cls) => Hir::class(cls),
        HirKind::Look(x) => Hir::look(x),
        HirKind::Repetition(mut x) => {
            x.sub = Box::new(renumber_capture_indices(*x.sub)?);
            Hir::repetition(x)
        }
        HirKind::Capture(mut cap) => {
            cap.index = match cap.index.checked_add(1) {
                Some(index) => index,
                None => {
                    // 此错误消息有点糟糕，但它可能是不可能发生的。
                    // 捕获索引溢出加法的唯一方法是正则表达式很大（或者其他事情出错了）。
                    let msg = "无法重新编号捕获索引，太大";
                    return Err(Error::any(msg));
                }
            };
            cap.sub = Box::new(renumber_capture_indices(*cap.sub)?);
            Hir::capture(cap)
        }
        HirKind::Concat(subs) => {
            let subs = subs
                .into_iter()
                .map(|sub| renumber_capture_indices(sub))
                .collect::<Result<Vec<Hir>, Error>>()?;
            Hir::concat(subs)
        }
        HirKind::Alternation(subs) => {
            let subs = subs
                .into_iter()
                .map(|sub| renumber_capture_indices(sub))
                .collect::<Result<Vec<Hir>, Error>>()?;
            Hir::alternation(subs)
        }
    })
}

/// 如果给定的字面字符串包含来自给定行终止符的任何字节，则返回true。
fn has_line_terminator(lineterm: LineTerminator, literal: &str) -> bool {
    if lineterm.is_crlf() {
        literal.as_bytes().iter().copied().any(|b| b == b'\r' || b == b'\n')
    } else {
        literal.as_bytes().iter().copied().any(|b| b == lineterm.as_byte())
    }
}
