//! Little-endian reader that returns `None` instead of panicking on short input.

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub(crate) fn rest(&self) -> &'a [u8] {
        self.buf
    }

    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.buf.len() < n {
            return None;
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Some(head)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.array::<1>().map(|b| b[0])
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        self.array().map(u16::from_le_bytes)
    }

    pub(crate) fn i16(&mut self) -> Option<i16> {
        self.array().map(i16::from_le_bytes)
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }

    pub(crate) fn i32(&mut self) -> Option<i32> {
        self.array().map(i32::from_le_bytes)
    }

    /// `n` bytes of UTF-16LE.
    pub(crate) fn utf16(&mut self, n: usize) -> Option<String> {
        let bytes = self.take(n)?;
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        Some(String::from_utf16_lossy(&units).trim_end_matches('\0').to_owned())
    }
}

/// UTF-16LE without a terminator.
pub(crate) fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}
