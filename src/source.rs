//! Buffered reader over a response body (already-read header leftovers, the
//! socket, de-chunked if needed).

use crate::http::Body;
use std::io::{self, Read};

pub struct Source {
    body: Body,
    buf: Vec<u8>,
    pos: usize,
}

impl Source {
    pub fn new(body: Body) -> Self {
        Source {
            body,
            buf: Vec::new(),
            pos: 0,
        }
    }

    /// Ensure at least one unread byte is buffered. Returns false on EOF.
    fn fill(&mut self) -> io::Result<bool> {
        if self.pos < self.buf.len() {
            return Ok(true);
        }
        let mut tmp = [0u8; 16384];
        let n = self.body.read(&mut tmp)?;
        if n == 0 {
            return Ok(false);
        }
        self.buf.clear();
        self.buf.extend_from_slice(&tmp[..n]);
        self.pos = 0;
        Ok(true)
    }

    /// Look at up to `n` upcoming bytes without consuming them (fewer at EOF).
    pub fn peek(&mut self, n: usize) -> io::Result<&[u8]> {
        while self.buf.len() - self.pos < n {
            let mut tmp = [0u8; 4096];
            let k = self.body.read(&mut tmp)?;
            if k == 0 {
                break;
            }
            self.buf.extend_from_slice(&tmp[..k]);
        }
        let end = (self.pos + n).min(self.buf.len());
        Ok(&self.buf[self.pos..end])
    }

    /// Read exactly `n` bytes into `out`. Returns false if EOF hit first, in
    /// which case `out` holds the partial data that did arrive.
    pub fn read_exact_n(&mut self, n: usize, out: &mut Vec<u8>) -> io::Result<bool> {
        out.clear();
        // `n` can come from the server (icy-metaint); it is validated upstream,
        // but never let it size an allocation up front.
        out.reserve(n.min(64 * 1024));
        while out.len() < n {
            if !self.fill()? {
                return Ok(false);
            }
            let avail = &self.buf[self.pos..];
            let take = (n - out.len()).min(avail.len());
            out.extend_from_slice(&avail[..take]);
            self.pos += take;
        }
        Ok(true)
    }

    pub fn read_byte(&mut self) -> io::Result<Option<u8>> {
        if !self.fill()? {
            return Ok(None);
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(Some(b))
    }

    /// Read whatever is currently buffered/available (for no-metadata streams).
    pub fn read_some(&mut self, out: &mut Vec<u8>) -> io::Result<bool> {
        out.clear();
        if !self.fill()? {
            return Ok(false);
        }
        out.extend_from_slice(&self.buf[self.pos..]);
        self.pos = self.buf.len();
        Ok(true)
    }
}
