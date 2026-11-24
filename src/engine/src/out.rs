use alloc::vec::Vec;

pub trait ByteSink {
    fn push(&mut self, b: u8);
    fn write_slice(&mut self, s: &[u8]);
    fn reserve_hint(&mut self, _extra: usize) {}
}

impl ByteSink for Vec<u8> {
    #[inline(always)]
    fn push(&mut self, b: u8) {
        Vec::push(self, b);
    }

    #[inline(always)]
    fn write_slice(&mut self, s: &[u8]) {
        self.extend_from_slice(s);
    }

    #[inline(always)]
    fn reserve_hint(&mut self, extra: usize) {
        self.reserve(extra);
    }
}

pub struct OutBuf<'a, F: FnMut(&[u8])> {
    buf: &'a mut [u8],
    pos: usize,
    flush_fn: F,
}

impl<'a, F: FnMut(&[u8])> OutBuf<'a, F> {
    #[inline]
    pub fn new(buf: &'a mut [u8], flush_fn: F) -> Self {
        Self {
            buf,
            pos: 0,
            flush_fn,
        }
    }

    #[inline]
    pub fn flush(&mut self) {
        if self.pos > 0 {
            (self.flush_fn)(&self.buf[..self.pos]);
            self.pos = 0;
        }
    }

    #[cold]
    #[inline(never)]
    fn write_slice_slow(&mut self, s: &[u8]) {
        self.flush();
        if s.len() >= self.buf.len() {
            (self.flush_fn)(s);
        } else {
            unsafe {
                core::ptr::copy_nonoverlapping(s.as_ptr(), self.buf.as_mut_ptr(), s.len());
            }
            self.pos = s.len();
        }
    }
}

impl<'a, F: FnMut(&[u8])> ByteSink for OutBuf<'a, F> {
    #[inline(always)]
    fn push(&mut self, b: u8) {
        if self.pos >= self.buf.len() {
            self.flush();
        }
        unsafe {
            *self.buf.get_unchecked_mut(self.pos) = b;
        }
        self.pos += 1;
    }

    #[inline(always)]
    fn write_slice(&mut self, s: &[u8]) {
        let rem = self.buf.len() - self.pos;
        if s.len() <= rem {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    s.as_ptr(),
                    self.buf.as_mut_ptr().add(self.pos),
                    s.len(),
                );
            }
            self.pos += s.len();
        } else {
            self.write_slice_slow(s);
        }
    }
}
