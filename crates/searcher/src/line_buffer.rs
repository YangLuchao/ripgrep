use std::cmp;
use std::io;

use bstr::ByteSlice;

/// 我们为行缓冲区使用的默认缓冲区容量。
pub(crate) const DEFAULT_BUFFER_CAPACITY: usize = 64 * (1 << 10); // 64 KB

/// 当面对长行和大上下文时，搜索器的行为。
///
/// 在使用固定大小缓冲区逐步搜索数据时，这控制了除缓冲区大小外额外分配的内存量，
/// 以容纳不适合缓冲区的行（在启用上下文窗口时可能包括上下文中的行）。
///
/// 默认情况下，急切地分配内存而没有限制。
#[derive(Clone, Copy, Debug)]
pub enum BufferAllocation {
    /// 尝试扩展缓冲区的大小，直到至少下一行适合内存，或者直到所有可用内存耗尽。
    ///
    /// 这是默认值。
    Eager,
    /// 将额外分配的内存量限制为给定的大小。如果找到需要比此处允许的更多内存的行，
    /// 则停止读取并返回错误。
    Error(usize),
}

impl Default for BufferAllocation {
    fn default() -> BufferAllocation {
        BufferAllocation::Eager
    }
}

/// 创建一个新的错误，用于在达到配置的分配限制时使用。
pub fn alloc_error(limit: usize) -> io::Error {
    let msg = format!("超过配置的分配限制（{}）", limit);
    io::Error::new(io::ErrorKind::Other, msg)
}

/// 二进制检测在行缓冲区中的行为。
///
/// 二进制检测是一种根据启发式方法识别给定数据块是否为二进制的过程，
/// 然后根据该启发式方法的结果采取行动。检测二进制数据的动机是二进制数据通常表示不希望使用文本模式进行搜索的数据。
/// 当然，也有许多情况不是这样，这就是为什么默认情况下禁用二进制检测的原因。
#[derive(Clone, Copy, Debug)]
pub enum BinaryDetection {
    /// 不执行二进制检测。行缓冲区报告的数据可能包含任意字节。
    None,
    /// 在所有被行缓冲区读取的内容中搜索给定的字节。
    /// 如果出现该字节，则将数据视为二进制数据，并使行缓冲区像到达 EOF 一样操作。
    /// 行缓冲区保证该字节永远不会被调用者观察到。
    Quit(u8),
    /// 在所有被行缓冲区读取的内容中搜索给定的字节。
    /// 如果出现该字节，则将其替换为行终止符。
    /// 行缓冲区保证该字节永远不会被调用者观察到。
    Convert(u8),
}

impl Default for BinaryDetection {
    fn default() -> BinaryDetection {
        BinaryDetection::None
    }
}

impl BinaryDetection {
    /// 当且仅当检测启发式要求行缓冲区一旦观察到二进制数据就停止读取数据时，返回 true。
    fn is_quit(&self) -> bool {
        match *self {
            BinaryDetection::Quit(_) => true,
            _ => false,
        }
    }
}

/// 缓冲区的配置。这包含一旦构造出缓冲区后就固定的选项。
#[derive(Clone, Copy, Debug)]
struct Config {
    /// 一次尝试读取的字节数。
    capacity: usize,
    /// 行终止符。
    lineterm: u8,
    /// 处理长行的行为。
    buffer_alloc: BufferAllocation,
    /// 当设置时，给定字节的存在表示二进制内容。
    binary: BinaryDetection,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            capacity: DEFAULT_BUFFER_CAPACITY,
            lineterm: b'\n',
            buffer_alloc: BufferAllocation::default(),
            binary: BinaryDetection::default(),
        }
    }
}

/// 用于构建行缓冲区的构建器。
#[derive(Clone, Debug, Default)]
pub struct LineBufferBuilder {
    config: Config,
}

impl LineBufferBuilder {
    /// 创建一个新的缓冲区构建器。
    pub fn new() -> LineBufferBuilder {
        LineBufferBuilder { config: Config::default() }
    }

    /// 根据此构建器的配置创建一个新的行缓冲区。
    pub fn build(&self) -> LineBuffer {
        LineBuffer {
            config: self.config,
            buf: vec![0; self.config.capacity],
            pos: 0,
            last_lineterm: 0,
            end: 0,
            absolute_byte_offset: 0,
            binary_byte_offset: None,
        }
    }

    /// 将用于缓冲区的默认容量设置为给定值。
    ///
    /// 一般来说，缓冲区的容量对应于要在内存中保存的数据量，
    /// 以及对底层读取器要执行的读取大小。
    ///
    /// 默认情况下，这设置为一个合理的默认值，除非有特定原因要更改它。
    pub fn capacity(&mut self, capacity: usize) -> &mut LineBufferBuilder {
        self.config.capacity = capacity;
        self
    }

    /// 设置缓冲区的行终止符。
    ///
    /// 每个缓冲区都有一个行终止符，该行终止符用于确定如何滚动缓冲区。
    /// 例如，当读取到缓冲区的底层读取器时，读取的数据结尾很可能对应于不完整的行。
    /// 作为行缓冲区，调用者不应访问此数据，因为它是不完整的。
    /// 行终止符是行缓冲区确定不完整读取部分的方式。
    ///
    /// 默认情况下，此设置为 `b'\n'`。
    pub fn line_terminator(&mut self, lineterm: u8) -> &mut LineBufferBuilder {
        self.config.lineterm = lineterm;
        self
    }

    /// 设置为适应长行的最大附加内存分配量。
    ///
    /// 为了启用面向行的搜索，基本要求是，至少每行都必须能够适合内存中。
    /// 此设置控制允许行的大小。默认情况下，设置为 `BufferAllocation::Eager`，
    /// 这意味着行缓冲区将尝试分配尽可能多的内存来适应一行，只受可用内存的限制。
    ///
    /// 请注意，此设置仅适用于*额外*分配的内存量，超出缓冲区容量。
    /// 这意味着值为 `0` 是明智的，特别是将确保行缓冲区永远不会在初始容量之外分配额外的内存。
    pub fn buffer_alloc(
        &mut self,
        behavior: BufferAllocation,
    ) -> &mut LineBufferBuilder {
        self.config.buffer_alloc = behavior;
        self
    }

    /// 是否启用二进制检测。根据设置，这可能会导致行缓冲区提早报告 EOF，或者可能会使行缓冲区清理数据。
    ///
    /// 默认情况下，此处禁用二进制检测。通常情况下，应将二进制检测视为不完美的启发式方法。
    pub fn binary_detection(
        &mut self,
        detection: BinaryDetection,
    ) -> &mut LineBufferBuilder {
        self.config.binary = detection;
        self
    }
}
/// 一个行缓冲读取器从任意读取器高效地读取面向行的缓冲区。
#[derive(Debug)]
pub struct LineBufferReader<'b, R> {
    rdr: R,
    line_buffer: &'b mut LineBuffer,
}

impl<'b, R: io::Read> LineBufferReader<'b, R> {
    /// 创建一个新的缓冲读取器，从`rdr`读取数据，并使用给定的`line_buffer`作为中间缓冲区。
    ///
    /// 这不会改变给定行缓冲区的二进制检测行为。
    pub fn new(
        rdr: R,
        line_buffer: &'b mut LineBuffer,
    ) -> LineBufferReader<'b, R> {
        line_buffer.clear();
        LineBufferReader { rdr, line_buffer }
    }

    /// 与`buffer`返回的数据相对应的绝对字节偏移量，相对于底层读取器内容的开头偏移量。
    /// 因此，此偏移量通常不对应于内存中的偏移量。它通常用于报告目的，也可用于计算已搜索的字节数。
    pub fn absolute_byte_offset(&self) -> u64 {
        self.line_buffer.absolute_byte_offset()
    }

    /// 如果检测到二进制数据，则返回最初检测到二进制数据的绝对字节偏移量。
    pub fn binary_byte_offset(&self) -> Option<u64> {
        self.line_buffer.binary_byte_offset()
    }

    /// 通过丢弃已消耗的缓冲区部分来填充此缓冲区的内容。
    /// 由丢弃已消耗的缓冲区部分创建的空闲空间然后由来自读取器的新数据填充。
    ///
    /// 如果达到EOF，则返回`false`。否则，返回`true`。
    /// （请注意，如果此行缓冲区的二进制检测设置为`Quit`，则存在二进制数据将导致此缓冲区的行为像在第一次出现二进制数据时看到EOF一样。）
    ///
    /// 这会转发底层读取器返回的任何错误，并且如果必须扩展缓冲区以超过其分配限制，则还会返回错误，如缓冲区分配策略所定义。
    pub fn fill(&mut self) -> Result<bool, io::Error> {
        self.line_buffer.fill(&mut self.rdr)
    }

    /// 返回此缓冲区的内容。
    pub fn buffer(&self) -> &[u8] {
        self.line_buffer.buffer()
    }

    /// 将缓冲区作为BStr返回，仅用于测试中的方便性相等性检查。
    #[cfg(test)]
    fn bstr(&self) -> &::bstr::BStr {
        self.buffer().as_bstr()
    }

    /// 消耗提供的字节数。这必须小于或等于由`buffer`返回的字节数。
    pub fn consume(&mut self, amt: usize) {
        self.line_buffer.consume(amt);
    }

    /// 消耗缓冲区的其余部分。在重新填充缓冲区之前，后续对`buffer`的调用保证返回一个空切片。
    ///
    /// 这是`consume(buffer.len())`的方便函数。
    #[cfg(test)]
    fn consume_all(&mut self) {
        self.line_buffer.consume_all();
    }
}

/// 一个行缓冲区管理（通常固定的）缓冲区以保存行。
///
/// 调用者应该节省地创建行缓冲区，并在可能的情况下重复使用它们。
/// 不能直接使用行缓冲区，而必须通过LineBufferReader使用。
#[derive(Clone, Debug)]
pub struct LineBuffer {
    /// 此缓冲区的配置。
    config: Config,
    /// 用于保存数据的主要缓冲区。
    buf: Vec<u8>,
    /// 此缓冲区的当前位置。这始终是`buf`中的有效切片索引，其最大值为`buf`的长度。
    pos: usize,
    /// 此缓冲区中可搜索内容的结束位置。
    /// 这要么设置为位于缓冲区中的最后一行终止符之后，要么设置为当读取器耗尽时最后一个字节之后。
    last_lineterm: usize,
    /// 缓冲区的结束位置。始终大于或等于`last_lineterm`。
    /// 在`last_lineterm`和`end`之间（如果有的话）的字节始终对应于部分行。
    end: usize,
    /// 相对于所有通过行缓冲区的数据（自构造或自上次调用`clear`以来）的绝对字节偏移量。
    /// 当行缓冲区到达EOF时，这设置为刚刚在底层读取器中读取的最后一个字节之后的位置。
    /// 即，它成为已读取的总字节数。
    absolute_byte_offset: u64,
    /// 如果检测到二进制数据，则记录最初检测到二进制数据的绝对字节偏移量。
    binary_byte_offset: Option<u64>,
}

impl LineBuffer {
    /// 在此行缓冲区上设置二进制检测方法。
    ///
    /// 这允许在不需要创建新的行缓冲区的情况下动态更改现有行缓冲区上的二进制检测策略。
    pub fn set_binary_detection(&mut self, binary: BinaryDetection) {
        self.config.binary = binary;
    }

    /// 重置此缓冲区，以便可以与新的读取器一起使用。
    fn clear(&mut self) {
        self.pos = 0;
        self.last_lineterm = 0;
        self.end = 0;
        self.absolute_byte_offset = 0;
        self.binary_byte_offset = None;
    }

    /// 与`buffer`返回的数据相对应的绝对字节偏移量，相对于读取器内容的开头偏移量。
    /// 因此，此偏移量通常不对应于内存中的偏移量。通常用于报告目的，特别是在错误消息中使用。
    ///
    /// 在调用`clear`时，这将重置为`0`。
    fn absolute_byte_offset(&self) -> u64 {
        self.absolute_byte_offset
    }

    /// 如果检测到二进制数据，则返回最初检测到二进制数据的绝对字节偏移量。
    fn binary_byte_offset(&self) -> Option<u64> {
        self.binary_byte_offset
    }

    /// 返回此缓冲区的内容。
    fn buffer(&self) -> &[u8] {
        &self.buf[self.pos..self.last_lineterm]
    }

    /// 将缓冲区的空闲空间内容作为可变切片返回。
    fn free_buffer(&mut self) -> &mut [u8] {
        &mut self.buf[self.end..]
    }

    /// 消耗提供的字节数。这必须小于或等于由`buffer`返回的字节数。
    fn consume(&mut self, amt: usize) {
        assert!(amt <= self.buffer().len());
        self.pos += amt;
        self.absolute_byte_offset += amt as u64;
    }

    /// 消耗缓冲区的其余部分。在重新填充缓冲区之前，后续对`buffer`的调用保证返回一个空切片。
    ///
    /// 这是`consume(buffer.len())`的方便函数。
    #[cfg(test)]
    fn consume_all(&mut self) {
        let amt = self.buffer().len();
        self.consume(amt);
    }

    /// 通过丢弃已消耗的缓冲区部分来填充此缓冲区的内容。
    /// 然后，通过从给定的读取器中填充新数据来创建已消耗部分的空闲空间。
    ///
    /// 在后续的fill调用中，调用者应该为此行缓冲区提供相同的读取器。只有在调用`clear`后，才能使用不同的读取器。
    ///
    /// 如果达到EOF，则返回`false`。否则，返回`true`。
    /// （请注意，如果此行缓冲区的二进制检测设置为`Quit`，则存在二进制数据将导致此缓冲区的行为像已看到EOF一样。）
    ///
    /// 此函数会转发由`rdr`返回的任何错误，并且如果必须扩展缓冲区以超过其分配限制，则还会返回错误，正如由缓冲区分配策略所规定。
    fn fill<R: io::Read>(&mut self, mut rdr: R) -> Result<bool, io::Error> {
        // 如果二进制检测启发告诉我们一旦观察到二进制数据就退出，则不再读取新数据，一旦当前缓冲区已被消耗，就会达到EOF。
        if self.config.binary.is_quit() && self.binary_byte_offset.is_some() {
            return Ok(!self.buffer().is_empty());
        }

        self.roll();
        assert_eq!(self.pos, 0);
        loop {
            self.ensure_capacity()?;
            let readlen = rdr.read(self.free_buffer().as_bytes_mut())?;
            if readlen == 0 {
                // 只有在调用者消耗了所有内容后，我们才完成了永久性的读取。
                self.last_lineterm = self.end;
                return Ok(!self.buffer().is_empty());
            }

            // 获取对刚刚读取的字节的可变视图。这些是我们在其中进行二进制检测的字节，也是我们搜索以查找最后一个行终止符的字节。
            // 在进行二进制转换的情况下，我们需要一个可变的切片。
            let oldend = self.end;
            self.end += readlen;
            let newbytes = &mut self.buf[oldend..self.end];

            // 二进制检测。
            match self.config.binary {
                BinaryDetection::None => {} // 无需执行任何操作
                BinaryDetection::Quit(byte) => {
                    if let Some(i) = newbytes.find_byte(byte) {
                        self.end = oldend + i;
                        self.last_lineterm = self.end;
                        self.binary_byte_offset =
                            Some(self.absolute_byte_offset + self.end as u64);
                        // 如果我们缓冲区中的第一个字节是二进制字节，则我们的缓冲区为空，应向调用者报告。
                        return Ok(self.pos < self.end);
                    }
                }
                BinaryDetection::Convert(byte) => {
                    if let Some(i) =
                        replace_bytes(newbytes, byte, self.config.lineterm)
                    {
                        // 仅记录第一个二进制偏移量。
                        if self.binary_byte_offset.is_none() {
                            self.binary_byte_offset = Some(
                                self.absolute_byte_offset
                                    + (oldend + i) as u64,
                            );
                        }
                    }
                }
            }

            // 如果读取到行终止符，则更新我们的`last_lineterm`位置。
            if let Some(i) = newbytes.rfind_byte(self.config.lineterm) {
                self.last_lineterm = oldend + i + 1;
                return Ok(true);
            }
            // 到此为止，如果找不到行终止符，则我们没有完整的行。因此，我们尝试读取更多！
        }
    }

    /// 通过丢弃已消耗的缓冲区部分来将缓冲区的未消耗部分滚动到前面。
    ///
    /// 此操作是幂等的。
    ///
    /// 滚动后，`last_lineterm`和`end`指向相同的位置，而`pos`始终设置为`0`。
    fn roll(&mut self) {
        if self.pos == self.end {
            self.pos = 0;
            self.last_lineterm = 0;
            self.end = 0;
            return;
        }

        let roll_len = self.end - self.pos;
        self.buf.copy_within(self.pos..self.end, 0);
        self.pos = 0;
        self.last_lineterm = roll_len;
        self.end = roll_len;
    }

    /// 确保内部缓冲区具有非零数量的可用空间以读取更多数据。
    /// 如果没有可用空间，则进行更多的分配。如果分配必须超过配置的限制，则返回错误。
    fn ensure_capacity(&mut self) -> Result<(), io::Error> {
        if !self.free_buffer().is_empty() {
            return Ok(());
        }
        // `len`用于计算下一个分配大小。容量允许从`0`开始，因此我们确保它至少为`1`。
        let len = cmp::max(1, self.buf.len());
        let additional = match self.config.buffer_alloc {
            BufferAllocation::Eager => len * 2,
            BufferAllocation::Error(limit) => {
                let used = self.buf.len() - self.config.capacity;
                let n = cmp::min(len * 2, limit - used);
                if n == 0 {
                    return Err(alloc_error(self.config.capacity + limit));
                }
                n
            }
        };
        assert!(additional > 0);
        let newlen = self.buf.len() + additional;
        self.buf.resize(newlen, 0);
        assert!(!self.free_buffer().is_empty());
        Ok(())
    }
}

/// Replaces `src` with `replacement` in bytes, and return the offset of the
/// first replacement, if one exists.
fn replace_bytes(bytes: &mut [u8], src: u8, replacement: u8) -> Option<usize> {
    if src == replacement {
        return None;
    }
    let mut first_pos = None;
    let mut pos = 0;
    while let Some(i) = bytes[pos..].find_byte(src).map(|i| pos + i) {
        if first_pos.is_none() {
            first_pos = Some(i);
        }
        bytes[i] = replacement;
        pos = i + 1;
        while bytes.get(pos) == Some(&src) {
            bytes[pos] = replacement;
            pos += 1;
        }
    }
    first_pos
}

#[cfg(test)]
mod tests {
    use super::*;
    use bstr::{ByteSlice, ByteVec};
    use std::str;

    const SHERLOCK: &'static str = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.\
";

    fn s(slice: &str) -> String {
        slice.to_string()
    }

    fn replace_str(
        slice: &str,
        src: u8,
        replacement: u8,
    ) -> (String, Option<usize>) {
        let mut dst = Vec::from(slice);
        let result = replace_bytes(&mut dst, src, replacement);
        (dst.into_string().unwrap(), result)
    }

    #[test]
    fn replace() {
        assert_eq!(replace_str("abc", b'b', b'z'), (s("azc"), Some(1)));
        assert_eq!(replace_str("abb", b'b', b'z'), (s("azz"), Some(1)));
        assert_eq!(replace_str("aba", b'a', b'z'), (s("zbz"), Some(0)));
        assert_eq!(replace_str("bbb", b'b', b'z'), (s("zzz"), Some(0)));
        assert_eq!(replace_str("bac", b'b', b'z'), (s("zac"), Some(0)));
    }

    #[test]
    fn buffer_basics1() {
        let bytes = "homer\nlisa\nmaggie";
        let mut linebuf = LineBufferBuilder::new().build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.buffer().is_empty());

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\nlisa\n");
        assert_eq!(rdr.absolute_byte_offset(), 0);
        rdr.consume(5);
        assert_eq!(rdr.absolute_byte_offset(), 5);
        rdr.consume_all();
        assert_eq!(rdr.absolute_byte_offset(), 11);

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "maggie");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), None);
    }

    #[test]
    fn buffer_basics2() {
        let bytes = "homer\nlisa\nmaggie\n";
        let mut linebuf = LineBufferBuilder::new().build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\nlisa\nmaggie\n");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), None);
    }

    #[test]
    fn buffer_basics3() {
        let bytes = "\n";
        let mut linebuf = LineBufferBuilder::new().build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "\n");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), None);
    }

    #[test]
    fn buffer_basics4() {
        let bytes = "\n\n";
        let mut linebuf = LineBufferBuilder::new().build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "\n\n");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), None);
    }

    #[test]
    fn buffer_empty() {
        let bytes = "";
        let mut linebuf = LineBufferBuilder::new().build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), None);
    }

    #[test]
    fn buffer_zero_capacity() {
        let bytes = "homer\nlisa\nmaggie";
        let mut linebuf = LineBufferBuilder::new().capacity(0).build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        while rdr.fill().unwrap() {
            rdr.consume_all();
        }
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), None);
    }

    #[test]
    fn buffer_small_capacity() {
        let bytes = "homer\nlisa\nmaggie";
        let mut linebuf = LineBufferBuilder::new().capacity(1).build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        let mut got = vec![];
        while rdr.fill().unwrap() {
            got.push_str(rdr.buffer());
            rdr.consume_all();
        }
        assert_eq!(bytes, got.as_bstr());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), None);
    }

    #[test]
    fn buffer_limited_capacity1() {
        let bytes = "homer\nlisa\nmaggie";
        let mut linebuf = LineBufferBuilder::new()
            .capacity(1)
            .buffer_alloc(BufferAllocation::Error(5))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\n");
        rdr.consume_all();

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "lisa\n");
        rdr.consume_all();

        // This returns an error because while we have just enough room to
        // store maggie in the buffer, we *don't* have enough room to read one
        // more byte, so we don't know whether we're at EOF or not, and
        // therefore must give up.
        assert!(rdr.fill().is_err());

        // We can mush on though!
        assert_eq!(rdr.bstr(), "m");
        rdr.consume_all();

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "aggie");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
    }

    #[test]
    fn buffer_limited_capacity2() {
        let bytes = "homer\nlisa\nmaggie";
        let mut linebuf = LineBufferBuilder::new()
            .capacity(1)
            .buffer_alloc(BufferAllocation::Error(6))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\n");
        rdr.consume_all();

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "lisa\n");
        rdr.consume_all();

        // We have just enough space.
        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "maggie");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
    }

    #[test]
    fn buffer_limited_capacity3() {
        let bytes = "homer\nlisa\nmaggie";
        let mut linebuf = LineBufferBuilder::new()
            .capacity(1)
            .buffer_alloc(BufferAllocation::Error(0))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.fill().is_err());
        assert_eq!(rdr.bstr(), "");
    }

    #[test]
    fn buffer_binary_none() {
        let bytes = "homer\nli\x00sa\nmaggie\n";
        let mut linebuf = LineBufferBuilder::new().build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.buffer().is_empty());

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\nli\x00sa\nmaggie\n");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), None);
    }

    #[test]
    fn buffer_binary_quit1() {
        let bytes = "homer\nli\x00sa\nmaggie\n";
        let mut linebuf = LineBufferBuilder::new()
            .binary_detection(BinaryDetection::Quit(b'\x00'))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.buffer().is_empty());

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\nli");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), 8);
        assert_eq!(rdr.binary_byte_offset(), Some(8));
    }

    #[test]
    fn buffer_binary_quit2() {
        let bytes = "\x00homer\nlisa\nmaggie\n";
        let mut linebuf = LineBufferBuilder::new()
            .binary_detection(BinaryDetection::Quit(b'\x00'))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "");
        assert_eq!(rdr.absolute_byte_offset(), 0);
        assert_eq!(rdr.binary_byte_offset(), Some(0));
    }

    #[test]
    fn buffer_binary_quit3() {
        let bytes = "homer\nlisa\nmaggie\n\x00";
        let mut linebuf = LineBufferBuilder::new()
            .binary_detection(BinaryDetection::Quit(b'\x00'))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.buffer().is_empty());

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\nlisa\nmaggie\n");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64 - 1);
        assert_eq!(rdr.binary_byte_offset(), Some(bytes.len() as u64 - 1));
    }

    #[test]
    fn buffer_binary_quit4() {
        let bytes = "homer\nlisa\nmaggie\x00\n";
        let mut linebuf = LineBufferBuilder::new()
            .binary_detection(BinaryDetection::Quit(b'\x00'))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.buffer().is_empty());

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\nlisa\nmaggie");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64 - 2);
        assert_eq!(rdr.binary_byte_offset(), Some(bytes.len() as u64 - 2));
    }

    #[test]
    fn buffer_binary_quit5() {
        let mut linebuf = LineBufferBuilder::new()
            .binary_detection(BinaryDetection::Quit(b'u'))
            .build();
        let mut rdr = LineBufferReader::new(SHERLOCK.as_bytes(), &mut linebuf);

        assert!(rdr.buffer().is_empty());

        assert!(rdr.fill().unwrap());
        assert_eq!(
            rdr.bstr(),
            "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, s\
"
        );
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), 76);
        assert_eq!(rdr.binary_byte_offset(), Some(76));
        assert_eq!(SHERLOCK.as_bytes()[76], b'u');
    }

    #[test]
    fn buffer_binary_convert1() {
        let bytes = "homer\nli\x00sa\nmaggie\n";
        let mut linebuf = LineBufferBuilder::new()
            .binary_detection(BinaryDetection::Convert(b'\x00'))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.buffer().is_empty());

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\nli\nsa\nmaggie\n");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), Some(8));
    }

    #[test]
    fn buffer_binary_convert2() {
        let bytes = "\x00homer\nlisa\nmaggie\n";
        let mut linebuf = LineBufferBuilder::new()
            .binary_detection(BinaryDetection::Convert(b'\x00'))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.buffer().is_empty());

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "\nhomer\nlisa\nmaggie\n");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), Some(0));
    }

    #[test]
    fn buffer_binary_convert3() {
        let bytes = "homer\nlisa\nmaggie\n\x00";
        let mut linebuf = LineBufferBuilder::new()
            .binary_detection(BinaryDetection::Convert(b'\x00'))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.buffer().is_empty());

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\nlisa\nmaggie\n\n");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), Some(bytes.len() as u64 - 1));
    }

    #[test]
    fn buffer_binary_convert4() {
        let bytes = "homer\nlisa\nmaggie\x00\n";
        let mut linebuf = LineBufferBuilder::new()
            .binary_detection(BinaryDetection::Convert(b'\x00'))
            .build();
        let mut rdr = LineBufferReader::new(bytes.as_bytes(), &mut linebuf);

        assert!(rdr.buffer().is_empty());

        assert!(rdr.fill().unwrap());
        assert_eq!(rdr.bstr(), "homer\nlisa\nmaggie\n\n");
        rdr.consume_all();

        assert!(!rdr.fill().unwrap());
        assert_eq!(rdr.absolute_byte_offset(), bytes.len() as u64);
        assert_eq!(rdr.binary_byte_offset(), Some(bytes.len() as u64 - 2));
    }
}
