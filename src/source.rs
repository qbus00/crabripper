//! Buffered reader over a `Conn` that can also carry bytes already read while
//! parsing the HTTP response header.

use crate::conn::Conn;
use std::io;
use std::io::Read;

pub struct Source {
    conn: Conn,
    buf: Vec<u8>,
    pos: usize,
}

impl Source {
    pub fn new(conn: Conn, leftover: Vec<u8>) -> Self {
        Source {
            conn,
            buf: leftover,
            pos: 0,
        }
    }

    /// Ensure at least one unread byte is buffered. Returns false on EOF.
    fn fill(&mut self) -> io::Result<bool> {
        if self.pos < self.buf.len() {
            return Ok(true);
        }
        let mut tmp = [0u8; 16384];
        let n = self.conn.read(&mut tmp)?;
        if n == 0 {
            return Ok(false);
        }
        self.buf.clear();
        self.buf.extend_from_slice(&tmp[..n]);
        self.pos = 0;
        Ok(true)
    }

    /// Read exactly `n` bytes into `out`. Returns false if EOF hit first.
    pub fn read_exact_n(&mut self, n: usize, out: &mut Vec<u8>) -> io::Result<bool> {
        out.clear();
        out.reserve(n);
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
