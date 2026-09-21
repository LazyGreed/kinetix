//! A streaming SSE frame splitter that tolerates arbitrary transport chunk
//! boundaries (FR-2.12).
//!
//! Providers separate SSE frames with either `\n\n` or `\r\n\r\n`; a single
//! upstream TCP read may deliver half a frame, several frames, or split a
//! multi-byte UTF-8 sequence. `SseFramer` accumulates *raw bytes* (never doing
//! a lossy per-chunk conversion, which would corrupt a split code point),
//! normalizes CRLF to LF, and yields complete frames — never a partial one.
//! This is exercised by the protocol torture tests in `src/torture.rs`.

/// Default maximum size of one incomplete SSE frame. Large model payloads
/// should be split across normal SSE events; an unbounded single event is a
/// memory-amplification risk.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseFrameError {
    FrameTooLarge { pending_bytes: usize, limit: usize },
}

impl std::fmt::Display for SseFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SseFrameError::FrameTooLarge {
                pending_bytes,
                limit,
            } => write!(
                f,
                "upstream SSE frame exceeds {limit} byte limit ({pending_bytes} bytes buffered)"
            ),
        }
    }
}

/// Splits an inbound SSE byte stream into complete frames.
pub struct SseFramer {
    buffer: Vec<u8>,
    /// Offset already checked for a delimiter. We retain the last three bytes
    /// between pushes because the longest delimiter is CRLF CRLF (4 bytes).
    scan_from: usize,
    max_frame_bytes: usize,
}

impl Default for SseFramer {
    fn default() -> Self {
        Self::with_max_frame_bytes(DEFAULT_MAX_FRAME_BYTES)
    }
}

impl SseFramer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_frame_bytes(max_frame_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            scan_from: 0,
            max_frame_bytes: max_frame_bytes.max(1),
        }
    }

    /// Feed one transport chunk and return every complete frame. Delimiter
    /// scanning resumes near the previous tail rather than rescanning the
    /// entire accumulated frame on every tiny transport chunk.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, SseFrameError> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();

        loop {
            let Some((idx, delimiter_len)) = find_delimiter(&self.buffer, self.scan_from) else {
                if self.buffer.len() > self.max_frame_bytes {
                    let pending_bytes = self.buffer.len();
                    self.buffer.clear();
                    self.scan_from = 0;
                    return Err(SseFrameError::FrameTooLarge {
                        pending_bytes,
                        limit: self.max_frame_bytes,
                    });
                }
                self.scan_from = self.buffer.len().saturating_sub(3);
                break;
            };

            if idx > self.max_frame_bytes {
                let pending_bytes = idx;
                self.buffer.clear();
                self.scan_from = 0;
                return Err(SseFrameError::FrameTooLarge {
                    pending_bytes,
                    limit: self.max_frame_bytes,
                });
            }

            let frame_bytes: Vec<u8> = self.buffer.drain(..idx).collect();
            self.buffer.drain(..delimiter_len);
            self.scan_from = 0;

            if frame_bytes.is_empty() {
                continue;
            }
            let normalized = normalize_crlf(&frame_bytes);
            if normalized.is_empty() {
                continue;
            }
            frames.push(String::from_utf8_lossy(&normalized).to_string());
        }

        Ok(frames)
    }

    pub fn pending(&self) -> bool {
        !self.buffer.is_empty()
    }

    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Clear and return trailing incomplete data for diagnostics. Stream
    /// drivers should normally treat any non-empty value at EOF as truncation.
    pub fn flush(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        let rest = String::from_utf8_lossy(&normalize_crlf(&self.buffer)).to_string();
        self.buffer.clear();
        self.scan_from = 0;
        if rest.trim().is_empty() {
            None
        } else {
            Some(rest)
        }
    }
}

fn find_delimiter(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut i = start.min(bytes.len());
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < bytes.len()
            && bytes[i] == b'\r'
            && bytes[i + 1] == b'\n'
            && bytes[i + 2] == b'\r'
            && bytes[i + 3] == b'\n'
        {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

/// Normalize CRLF inside a complete frame. This only runs once per completed
/// frame, avoiding repeated whole-buffer normalization for partial chunks.
fn normalize_crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'\r' && bytes[i + 1] == b'\n' {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Extract the `data:` payload from one SSE frame, joining multiple data lines
/// with newlines per the SSE spec.
pub fn extract_data(frame: &str) -> Option<String> {
    let mut data = String::new();
    let mut found = false;
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            found = true;
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if found {
        Some(data)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_in_chunks(framer: &mut SseFramer, input: &str, size: usize) -> Vec<String> {
        let bytes = input.as_bytes();
        let mut out = Vec::new();
        for chunk in bytes.chunks(size) {
            out.extend(framer.push(chunk).unwrap());
        }
        out
    }

    #[test]
    fn splits_lf_frames() {
        let mut f = SseFramer::new();
        let frames = f.push(b"data: a\n\ndata: b\n\n").unwrap();
        assert_eq!(frames, vec!["data: a", "data: b"]);
        assert!(!f.pending());
    }

    #[test]
    fn splits_crlf_frames() {
        let mut f = SseFramer::new();
        let frames = f.push(b"data: a\r\n\r\ndata: b\r\n\r\n").unwrap();
        assert_eq!(frames, vec!["data: a", "data: b"]);
    }

    #[test]
    fn tolerates_one_byte_chunks() {
        let input = "data: {\"x\":1}\n\ndata: {\"y\":2}\n\n: keepalive\n\ndata: [DONE]\n\n";
        for size in 1..=8 {
            let mut f = SseFramer::new();
            let frames = feed_in_chunks(&mut f, input, size);
            assert_eq!(
                frames,
                vec![
                    "data: {\"x\":1}",
                    "data: {\"y\":2}",
                    ": keepalive",
                    "data: [DONE]"
                ],
                "chunk size {size}"
            );
        }
    }

    #[test]
    fn tolerates_split_utf8_multibyte() {
        // A multi-byte UTF-8 sequence split across two chunks must not corrupt.
        let input = "data: {\"t\":\"héllo→世界\"}\n\n";
        let bytes = input.as_bytes();
        for split in 1..bytes.len() {
            let mut f = SseFramer::new();
            let mut frames = f.push(&bytes[..split]).unwrap();
            frames.extend(f.push(&bytes[split..]).unwrap());
            assert_eq!(
                frames,
                vec!["data: {\"t\":\"héllo→世界\"}"],
                "split {split}"
            );
        }
    }

    #[test]
    fn crlf_split_across_chunks() {
        // The CRLF terminator split so that CR ends one chunk and LF starts the
        // next must still be recognized.
        let mut f = SseFramer::new();
        assert!(f.push(b"data: a\r\n\r").unwrap().is_empty());
        assert_eq!(
            f.push(b"\ndata: b\r\n\r\n").unwrap(),
            vec!["data: a", "data: b"]
        );
    }

    #[test]
    fn partial_frame_is_not_emitted() {
        let mut f = SseFramer::new();
        assert!(f.push(b"data: par").unwrap().is_empty());
        assert!(f.pending());
        let frames = f.push(b"tial\n\n").unwrap();
        assert_eq!(frames, vec!["data: partial"]);
    }

    #[test]
    fn joins_multiple_data_lines() {
        let mut f = SseFramer::new();
        let frames = f.push(b"data: line1\ndata: line2\n\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(extract_data(&frames[0]).unwrap(), "line1\nline2");
    }

    #[test]
    fn comments_have_no_data() {
        let mut f = SseFramer::new();
        let frames = f.push(b": keepalive\n\n").unwrap();
        assert_eq!(frames, vec![": keepalive"]);
        assert!(extract_data(&frames[0]).is_none());
    }
    #[test]
    fn reports_pending_byte_count() {
        let mut f = SseFramer::new();
        f.push(b"data: partial").unwrap();
        assert_eq!(f.pending_bytes(), 13);
        assert!(f.flush().is_some());
        assert_eq!(f.pending_bytes(), 0);
    }

    #[test]
    fn rejects_oversized_completed_frame() {
        let mut f = SseFramer::with_max_frame_bytes(8);
        let err = f.push(b"123456789\n\n").unwrap_err();
        assert!(matches!(
            err,
            SseFrameError::FrameTooLarge {
                pending_bytes: 9,
                limit: 8
            }
        ));
    }

    #[test]
    fn rejects_oversized_frame() {
        let mut f = SseFramer::with_max_frame_bytes(8);
        let err = f.push(b"123456789").unwrap_err();
        assert!(matches!(
            err,
            SseFrameError::FrameTooLarge {
                pending_bytes: 9,
                limit: 8
            }
        ));
        assert_eq!(f.pending_bytes(), 0);
    }

    #[test]
    fn adversarial_one_byte_chunks_stay_bounded() {
        let mut f = SseFramer::with_max_frame_bytes(64);
        for _ in 0..64 {
            assert!(f.push(b"x").unwrap().is_empty());
            assert!(f.pending_bytes() <= 64);
        }
        assert!(f.push(b"x").is_err());
    }
}
