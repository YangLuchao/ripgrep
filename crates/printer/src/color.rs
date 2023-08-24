use std::error;
use std::fmt;
use std::str::FromStr;

use termcolor::{Color, ColorSpec, ParseColorError};
/// 返回默认的颜色规范集合。
///
/// 这可能会随时间而变化，但颜色选择旨在保持相对保守，适用于各种终端主题。
///
/// 可以将其他颜色规范添加到返回的列表中。最近添加的规范将覆盖先前添加的规范。
pub fn default_color_specs() -> Vec<UserColorSpec> {
    vec![
        #[cfg(unix)]
        "path:fg:magenta".parse().unwrap(),
        #[cfg(windows)]
        "path:fg:cyan".parse().unwrap(),
        "line:fg:green".parse().unwrap(),
        "match:fg:red".parse().unwrap(),
        "match:style:bold".parse().unwrap(),
    ]
}

/// 解析颜色规范时可能出现的错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ColorError {
    /// 使用了未识别的输出类型时发生。
    UnrecognizedOutType(String),
    /// 使用了未识别的规范类型时发生。
    UnrecognizedSpecType(String),
    /// 使用了未识别的颜色名称时发生。
    UnrecognizedColor(String, String),
    /// 使用了未识别的样式属性时发生。
    UnrecognizedStyle(String),
    /// 颜色规范的格式无效时发生。
    InvalidFormat(String),
}

impl error::Error for ColorError {
    fn description(&self) -> &str {
        match *self {
            ColorError::UnrecognizedOutType(_) => "未识别的输出类型",
            ColorError::UnrecognizedSpecType(_) => "未识别的规范类型",
            ColorError::UnrecognizedColor(_, _) => "未识别的颜色名称",
            ColorError::UnrecognizedStyle(_) => "未识别的样式属性",
            ColorError::InvalidFormat(_) => "颜色规范格式无效",
        }
    }
}

impl ColorError {
    fn from_parse_error(err: ParseColorError) -> ColorError {
        ColorError::UnrecognizedColor(
            err.invalid().to_string(),
            err.to_string(),
        )
    }
}

impl fmt::Display for ColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ColorError::UnrecognizedOutType(ref name) => write!(
                f,
                "未识别的输出类型'{}'。可选择的类型有：\
                     path、line、column、match。",
                name,
            ),
            ColorError::UnrecognizedSpecType(ref name) => write!(
                f,
                "未识别的规范类型'{}'。可选择的类型有：\
                     fg、bg、style、none。",
                name,
            ),
            ColorError::UnrecognizedColor(_, ref msg) => write!(f, "{}", msg),
            ColorError::UnrecognizedStyle(ref name) => write!(
                f,
                "未识别的样式属性'{}'。可选择的属性有：\
                     nobold、bold、nointense、intense、nounderline、\
                     underline。",
                name,
            ),
            ColorError::InvalidFormat(ref original) => write!(
                f,
                "无效的颜色规范格式：'{}'。有效的格式为\
                     '(path|line|column|match):(fg|bg|style):(value)'。",
                original,
            ),
        }
    }
}

/// 合并的颜色规范集合。
///
/// 这组颜色规范表示此库中的打印机支持的各种颜色类型。可以从一系列
/// [`UserColorSpec`s](struct.UserColorSpec.html) 创建一组颜色规范。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ColorSpecs {
    path: ColorSpec,
    line: ColorSpec,
    column: ColorSpec,
    matched: ColorSpec,
}

/// 用户提供的单个颜色规范。
///
/// ## 格式
///
/// `UserColorSpec` 的格式是三元组：`{type}:{attribute}:{value}`。每个组件的定义如下：
///
/// * `{type}` 可以是 `path`、`line`、`column` 或 `match` 中的一个。
/// * `{attribute}` 可以是 `fg`、`bg` 或 `style` 中的一个。`{attribute}` 也可以是特殊值 `none`，
///   在这种情况下，可以省略 `{value}`。
/// * `{value}` 在 `{attribute}` 是 `fg` 或 `bg` 时应为颜色，或者在 `{attribute}` 是 `style` 时应为样式指令。
///   当 `{attribute}` 是 `none` 时，必须省略 `{value}`。
///
/// `{type}` 控制着在标准打印机中颜色应用的部分，例如文件路径还是行号等。
///
/// 当 `{attribute}` 为 `none` 时，这应该会清除指定 `type` 的任何现有样式设置。
///
/// 当 `{attribute}` 为 `fg` 或 `bg` 时，`{value}` 应为颜色；当 `{attribute}` 为 `style` 时，`{value}` 应为样式指令。
/// 当 `{attribute}` 为 `none` 时，必须省略 `{value}`。
///
/// 有效的颜色包括 `black`、`blue`、`green`、`red`、`cyan`、`magenta`、`yellow`、`white`。
/// 也可以指定扩展颜色，格式为 `x`（256 位颜色）或 `x,x,x`（24 位真彩色），其中 `x` 是介于 0 到 255 之间的数字，包含两端。
/// `x` 可以以普通十进制数或以 `0x` 为前缀的十六进制数给出。
///
/// 有效的样式指令包括 `nobold`、`bold`、`intense`、`nointense`、`underline`、`nounderline`。
///
/// ## 示例
///
/// 构建 `UserColorSpec` 的标准方式是从字符串解析它。一旦构建了多个 `UserColorSpec`，它们可以提供给标准打印机，
/// 在那里它们将自动应用于输出。
///
/// `UserColorSpec` 也可以转换为 `termcolor::ColorSpec`：
///
/// ```rust
/// # fn main() {
/// use termcolor::{Color, ColorSpec};
/// use grep_printer::UserColorSpec;
///
/// let user_spec1: UserColorSpec = "path:fg:blue".parse().unwrap();
/// let user_spec2: UserColorSpec = "match:bg:0xff,0x7f,0x00".parse().unwrap();
///
/// let spec1 = user_spec1.to_color_spec();
/// let spec2 = user_spec2.to_color_spec();
///
/// assert_eq!(spec1.fg(), Some(&Color::Blue));
/// assert_eq!(spec2.bg(), Some(&Color::Rgb(0xFF, 0x7F, 0x00)));
/// # }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserColorSpec {
    ty: OutType,
    value: SpecValue,
}

impl UserColorSpec {
    /// 将此用户提供的颜色规范转换为可用于 `termcolor` 的规范。这将丢弃此规范的类型
    /// （其中类型指示颜色在标准打印机中应用的位置，例如文件路径或行号等）。
    pub fn to_color_spec(&self) -> ColorSpec {
        let mut spec = ColorSpec::default();
        self.value.merge_into(&mut spec);
        spec
    }
}
/// 规范指定的实际值。
#[derive(Clone, Debug, Eq, PartialEq)]
enum SpecValue {
    None,
    Fg(Color),
    Bg(Color),
    Style(Style),
}

/// 可配置部分的集合，用于 ripgrep 的输出。
#[derive(Clone, Debug, Eq, PartialEq)]
enum OutType {
    Path,
    Line,
    Column,
    Match,
}

/// 规范类型。
#[derive(Clone, Debug, Eq, PartialEq)]
enum SpecType {
    Fg,
    Bg,
    Style,
    None,
}

/// 终端中可用的样式集合。
#[derive(Clone, Debug, Eq, PartialEq)]
enum Style {
    Bold,
    NoBold,
    Intense,
    NoIntense,
    Underline,
    NoUnderline,
}

impl ColorSpecs {
    /// 从用户提供的规范列表创建颜色规范。
    pub fn new(specs: &[UserColorSpec]) -> ColorSpecs {
        let mut merged = ColorSpecs::default();
        for spec in specs {
            match spec.ty {
                OutType::Path => spec.merge_into(&mut merged.path),
                OutType::Line => spec.merge_into(&mut merged.line),
                OutType::Column => spec.merge_into(&mut merged.column),
                OutType::Match => spec.merge_into(&mut merged.matched),
            }
        }
        merged
    }

    /// 创建一个带有颜色的默认规范集。
    ///
    /// 这与 `ColorSpecs` 的 `Default` 实现不同，因为它提供了一组默认的颜色选择，而 `Default`
    /// 实现则不提供任何颜色选择。
    pub fn default_with_color() -> ColorSpecs {
        ColorSpecs::new(&default_color_specs())
    }

    /// 返回用于着色文件路径的颜色规范。
    pub fn path(&self) -> &ColorSpec {
        &self.path
    }

    /// 返回用于着色行号的颜色规范。
    pub fn line(&self) -> &ColorSpec {
        &self.line
    }

    /// 返回用于着色列号的颜色规范。
    pub fn column(&self) -> &ColorSpec {
        &self.column
    }

    /// 返回用于着色匹配文本的颜色规范。
    pub fn matched(&self) -> &ColorSpec {
        &self.matched
    }
}

impl UserColorSpec {
    /// 将此规范合并到给定的颜色规范中。
    fn merge_into(&self, cspec: &mut ColorSpec) {
        self.value.merge_into(cspec);
    }
}

impl SpecValue {
    /// 将此规范值合并到给定的颜色规范中。
    fn merge_into(&self, cspec: &mut ColorSpec) {
        match *self {
            SpecValue::None => cspec.clear(),
            SpecValue::Fg(ref color) => {
                cspec.set_fg(Some(color.clone()));
            }
            SpecValue::Bg(ref color) => {
                cspec.set_bg(Some(color.clone()));
            }
            SpecValue::Style(ref style) => match *style {
                Style::Bold => {
                    cspec.set_bold(true);
                }
                Style::NoBold => {
                    cspec.set_bold(false);
                }
                Style::Intense => {
                    cspec.set_intense(true);
                }
                Style::NoIntense => {
                    cspec.set_intense(false);
                }
                Style::Underline => {
                    cspec.set_underline(true);
                }
                Style::NoUnderline => {
                    cspec.set_underline(false);
                }
            },
        }
    }
}

impl FromStr for UserColorSpec {
    type Err = ColorError;

    fn from_str(s: &str) -> Result<UserColorSpec, ColorError> {
        let pieces: Vec<&str> = s.split(':').collect();
        if pieces.len() <= 1 || pieces.len() > 3 {
            return Err(ColorError::InvalidFormat(s.to_string()));
        }
        let otype: OutType = pieces[0].parse()?;
        match pieces[1].parse()? {
            SpecType::None => {
                Ok(UserColorSpec { ty: otype, value: SpecValue::None })
            }
            SpecType::Style => {
                if pieces.len() < 3 {
                    return Err(ColorError::InvalidFormat(s.to_string()));
                }
                let style: Style = pieces[2].parse()?;
                Ok(UserColorSpec { ty: otype, value: SpecValue::Style(style) })
            }
            SpecType::Fg => {
                if pieces.len() < 3 {
                    return Err(ColorError::InvalidFormat(s.to_string()));
                }
                let color: Color =
                    pieces[2].parse().map_err(ColorError::from_parse_error)?;
                Ok(UserColorSpec { ty: otype, value: SpecValue::Fg(color) })
            }
            SpecType::Bg => {
                if pieces.len() < 3 {
                    return Err(ColorError::InvalidFormat(s.to_string()));
                }
                let color: Color =
                    pieces[2].parse().map_err(ColorError::from_parse_error)?;
                Ok(UserColorSpec { ty: otype, value: SpecValue::Bg(color) })
            }
        }
    }
}

impl FromStr for OutType {
    type Err = ColorError;

    fn from_str(s: &str) -> Result<OutType, ColorError> {
        match &*s.to_lowercase() {
            "path" => Ok(OutType::Path),
            "line" => Ok(OutType::Line),
            "column" => Ok(OutType::Column),
            "match" => Ok(OutType::Match),
            _ => Err(ColorError::UnrecognizedOutType(s.to_string())),
        }
    }
}

impl FromStr for SpecType {
    type Err = ColorError;

    fn from_str(s: &str) -> Result<SpecType, ColorError> {
        match &*s.to_lowercase() {
            "fg" => Ok(SpecType::Fg),
            "bg" => Ok(SpecType::Bg),
            "style" => Ok(SpecType::Style),
            "none" => Ok(SpecType::None),
            _ => Err(ColorError::UnrecognizedSpecType(s.to_string())),
        }
    }
}

impl FromStr for Style {
    type Err = ColorError;

    fn from_str(s: &str) -> Result<Style, ColorError> {
        match &*s.to_lowercase() {
            "bold" => Ok(Style::Bold),
            "nobold" => Ok(Style::NoBold),
            "intense" => Ok(Style::Intense),
            "nointense" => Ok(Style::NoIntense),
            "underline" => Ok(Style::Underline),
            "nounderline" => Ok(Style::NoUnderline),
            _ => Err(ColorError::UnrecognizedStyle(s.to_string())),
        }
    }
}
