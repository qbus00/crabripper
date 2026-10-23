//! Ogg logical-stream splitter for Ogg Vorbis.
//!
//! Icecast Ogg streams chain one logical bitstream per track (a new metadata set
//! needs a new Vorbis comment header, which only a new logical stream can carry).
//! So we split on **Beginning-Of-Stream (BOS) pages**, copying raw Ogg pages
//! verbatim — lossless, and every track is a valid standalone `.ogg` file. The
//! track title comes from the Vorbis comment header.

/// One output event: `title` is `Some(..)` when `bytes` begins a new track.
pub struct Event {
    pub title: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Default)]
pub struct OggSplitter {
    buf: Vec<u8>,
    started: bool,
    current_serial: u32,
    pending: Option<Pending>,
}

struct Pending {
    serial: u32,
    pages: Vec<u8>,
    packet: Vec<u8>,
    packets_done: usize,
    title: Option<String>,
}

struct PageInfo {
    total_len: usize,
    header_type: u8,
    serial: u32,
    body_offset: usize,
    seg_lens: Vec<u8>,
}

impl OggSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, data: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(data);
        let mut events = Vec::new();

        loop {
            // Resync to a page boundary if needed.
            if !self.buf.starts_with(b"OggS") {
                match find(&self.buf, b"OggS") {
                    Some(i) => {
                        self.buf.drain(..i);
                    }
                    None => {
                        // keep a small tail in case "OggS" straddles the next read
                        if self.buf.len() > 3 {
                            let keep = self.buf.len() - 3;
                            self.buf.drain(..keep);
                        }
                        break;
                    }
                }
            }
            let page = match read_page(&self.buf) {
                Some(p) => p,
                None => break, // incomplete page; wait for more bytes
            };
            let page_bytes = self.buf[..page.total_len].to_vec();
            self.buf.drain(..page.total_len);
            self.handle_page(page, page_bytes, &mut events);
        }
        events
    }

    fn handle_page(&mut self, page: PageInfo, page_bytes: Vec<u8>, events: &mut Vec<Event>) {
        let is_bos = page.header_type & 0x02 != 0;

        if let Some(p) = &mut self.pending {
            // Buffering a just-started logical stream until its title is known.
            if is_bos && page.serial != p.serial {
                // A new BOS before we resolved the current one: flush it first.
                self.flush_pending(events);
                self.start_pending(page, page_bytes);
                return;
            }
            reassemble(p, &page, &page_bytes);
            p.pages.extend_from_slice(&page_bytes);
            if p.title.is_some() || p.packets_done >= 3 || p.pages.len() > 262_144 {
                self.flush_pending(events);
            }
            return;
        }

        if is_bos {
            // New track begins here.
            self.start_pending(page, page_bytes);
        } else if self.started && page.serial == self.current_serial {
            // Continuation of the current track.
            events.push(Event {
                title: None,
                bytes: page_bytes,
            });
        }
        // else: pages before the first BOS (joined mid-song) — dropped.
    }

    fn start_pending(&mut self, page: PageInfo, page_bytes: Vec<u8>) {
        let mut p = Pending {
            serial: page.serial,
            pages: Vec::new(),
            packet: Vec::new(),
            packets_done: 0,
            title: None,
        };
        reassemble(&mut p, &page, &page_bytes);
        p.pages.extend_from_slice(&page_bytes);
        self.pending = Some(p);
    }

    fn flush_pending(&mut self, events: &mut Vec<Event>) {
        if let Some(p) = self.pending.take() {
            self.current_serial = p.serial;
            self.started = true;
            events.push(Event {
                title: Some(p.title.unwrap_or_default()),
                bytes: p.pages,
            });
        }
    }
}

/// Reassemble packets from a page's segment table; parse the Vorbis comment
/// header (packet starting with 0x03 "vorbis") for the title.
fn reassemble(p: &mut Pending, page: &PageInfo, page_bytes: &[u8]) {
    let mut off = page.body_offset;
    for &seg in &page.seg_lens {
        let seg = seg as usize;
        if off + seg <= page_bytes.len() {
            p.packet.extend_from_slice(&page_bytes[off..off + seg]);
        }
        off += seg;
        if seg < 255 {
            if p.title.is_none()
                && (p.packet.starts_with(b"\x03vorbis") || p.packet.starts_with(b"OpusTags"))
            {
                p.title = Some(parse_vorbis_comment(&p.packet));
            }
            p.packets_done += 1;
            p.packet.clear();
        }
    }
}

/// Extract "Artist - Title" (or just title) from a Vorbis/Opus comment packet.
fn parse_vorbis_comment(packet: &[u8]) -> String {
    // layout: <magic> | vendor_len(u32) vendor | count(u32) | (len(u32) "KEY=val")*
    // magic is "\x03vorbis" (7 bytes) for Vorbis, "OpusTags" (8 bytes) for Opus.
    let mut pos = if packet.starts_with(b"OpusTags") {
        8
    } else {
        7
    };
    let rd = |b: &[u8], p: usize| -> Option<u32> {
        if p + 4 <= b.len() {
            Some(u32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]))
        } else {
            None
        }
    };
    let vlen = match rd(packet, pos) {
        Some(v) => v as usize,
        None => return String::new(),
    };
    pos += 4 + vlen;
    let count = match rd(packet, pos) {
        Some(c) => c as usize,
        None => return String::new(),
    };
    pos += 4;
    let (mut artist, mut title) = (String::new(), String::new());
    for _ in 0..count {
        let len = match rd(packet, pos) {
            Some(l) => l as usize,
            None => break,
        };
        pos += 4;
        if pos + len > packet.len() {
            break;
        }
        let comment = String::from_utf8_lossy(&packet[pos..pos + len]);
        pos += len;
        if let Some((k, v)) = comment.split_once('=') {
            match k.to_ascii_uppercase().as_str() {
                "ARTIST" => artist = v.to_string(),
                "TITLE" => title = v.to_string(),
                _ => {}
            }
        }
    }
    match (artist.is_empty(), title.is_empty()) {
        (false, false) => format!("{artist} - {title}"),
        (true, false) => title,
        (false, true) => artist,
        _ => String::new(),
    }
}

fn read_page(buf: &[u8]) -> Option<PageInfo> {
    if buf.len() < 27 || &buf[0..4] != b"OggS" {
        return None;
    }
    let header_type = buf[5];
    let serial = u32::from_le_bytes([buf[14], buf[15], buf[16], buf[17]]);
    let nsegs = buf[26] as usize;
    if buf.len() < 27 + nsegs {
        return None;
    }
    let seg_lens = buf[27..27 + nsegs].to_vec();
    let body_len: usize = seg_lens.iter().map(|&b| b as usize).sum();
    let total = 27 + nsegs + body_len;
    if buf.len() < total {
        return None;
    }
    Some(PageInfo {
        total_len: total,
        header_type,
        serial,
        body_offset: 27 + nsegs,
        seg_lens,
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vorbis_comment() {
        // build a minimal comment packet
        let mut p = Vec::new();
        p.extend_from_slice(b"\x03vorbis");
        p.extend_from_slice(&3u32.to_le_bytes());
        p.extend_from_slice(b"ven");
        p.extend_from_slice(&2u32.to_le_bytes());
        for c in ["ARTIST=The Band", "TITLE=The Song"] {
            p.extend_from_slice(&(c.len() as u32).to_le_bytes());
            p.extend_from_slice(c.as_bytes());
        }
        assert_eq!(parse_vorbis_comment(&p), "The Band - The Song");
    }
}
