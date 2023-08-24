/*!
' grep-matcher '的' Matcher '特性在Rust的regex引擎中的实现。
*/
#![deny(missing_docs)]

pub use crate::error::{Error, ErrorKind};
pub use crate::matcher::{RegexCaptures, RegexMatcher, RegexMatcherBuilder};

mod ast;
mod config;
mod error;
mod literal;
mod matcher;
mod non_matching;
mod strip;
mod word;
