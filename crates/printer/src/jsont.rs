use std::borrow::Cow;
use std::path::Path;
use std::str;

use base64;
use serde::{Serialize, Serializer};

use crate::stats::Stats;

// 枚举：消息类型，用于JSON序列化
#[derive(Serialize)]
#[serde(tag = "type", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum Message<'a> {
    Begin(Begin<'a>),
    End(End<'a>),
    Match(Match<'a>),
    Context(Context<'a>),
}

// 结构体：开始消息
#[derive(Serialize)]
pub struct Begin<'a> {
    #[serde(serialize_with = "ser_path")]
    pub path: Option<&'a Path>,
}

// 结构体：结束消息
#[derive(Serialize)]
pub struct End<'a> {
    #[serde(serialize_with = "ser_path")]
    pub path: Option<&'a Path>,
    pub binary_offset: Option<u64>,
    pub stats: Stats,
}

// 结构体：匹配消息
#[derive(Serialize)]
pub struct Match<'a> {
    #[serde(serialize_with = "ser_path")]
    pub path: Option<&'a Path>,
    #[serde(serialize_with = "ser_bytes")]
    pub lines: &'a [u8],
    pub line_number: Option<u64>,
    pub absolute_offset: u64,
    pub submatches: &'a [SubMatch<'a>],
}

// 结构体：上下文消息
#[derive(Serialize)]
pub struct Context<'a> {
    #[serde(serialize_with = "ser_path")]
    pub path: Option<&'a Path>,
    #[serde(serialize_with = "ser_bytes")]
    pub lines: &'a [u8],
    pub line_number: Option<u64>,
    pub absolute_offset: u64,
    pub submatches: &'a [SubMatch<'a>],
}

// 结构体：子匹配项
#[derive(Serialize)]
pub struct SubMatch<'a> {
    #[serde(rename = "match")]
    #[serde(serialize_with = "ser_bytes")]
    pub m: &'a [u8],
    pub start: usize,
    pub end: usize,
}

// 枚举：数据表示类似字符串的内容，可能不是有效的UTF-8
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize)]
#[serde(untagged)]
enum Data<'a> {
    Text {
        text: Cow<'a, str>,
    },
    Bytes {
        #[serde(serialize_with = "to_base64")]
        bytes: &'a [u8],
    },
}

impl<'a> Data<'a> {
    fn from_bytes(bytes: &[u8]) -> Data<'_> {
        match str::from_utf8(bytes) {
            Ok(text) => Data::Text { text: Cow::Borrowed(text) },
            Err(_) => Data::Bytes { bytes },
        }
    }

    #[cfg(unix)]
    fn from_path(path: &Path) -> Data<'_> {
        use std::os::unix::ffi::OsStrExt;

        match path.to_str() {
            Some(text) => Data::Text { text: Cow::Borrowed(text) },
            None => Data::Bytes { bytes: path.as_os_str().as_bytes() },
        }
    }

    #[cfg(not(unix))]
    fn from_path(path: &Path) -> Data {
        // 使用lossy转换意味着某些路径可能无法精确地往返，
        // 但目前不清楚我们实际上应该做什么。
        // Serde 拒绝非UTF-8路径，而在Windows上，OsStr序列化为UTF-16代码单元序列。
        // 对于这种情况，都不合适，所以暂时简化处理。
        Data::Text { text: path.to_string_lossy() }
    }
}

// 将字节数组转换为Base64编码
fn to_base64<T, S>(bytes: T, ser: S) -> Result<S::Ok, S::Error>
where
    T: AsRef<[u8]>,
    S: Serializer,
{
    ser.serialize_str(&base64::encode(&bytes))
}

// 序列化字节数组
fn ser_bytes<T, S>(bytes: T, ser: S) -> Result<S::Ok, S::Error>
where
    T: AsRef<[u8]>,
    S: Serializer,
{
    Data::from_bytes(bytes.as_ref()).serialize(ser)
}

// 序列化路径
fn ser_path<P, S>(path: &Option<P>, ser: S) -> Result<S::Ok, S::Error>
where
    P: AsRef<Path>,
    S: Serializer,
{
    path.as_ref().map(|p| Data::from_path(p.as_ref())).serialize(ser)
}
