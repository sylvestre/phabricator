//! Build logs (`HarbormasterBuildLog`) as a step writes them:
//! `$target->newLog($source, $type)->append($text)`, which opens the log,
//! flushes the text into `harbormaster_buildlogchunk` rows of at most
//! `CHUNK_BYTE_LIMIT` bytes while keeping the line map, and closes it when
//! PHP destroys the object at the end of the statement (which queues a
//! `HarbormasterLogWorker` task).

use base64::Engine as _;
use serde_json::{Value, json};

/// `HarbormasterBuildLog::CHUNK_BYTE_LIMIT`.
pub(super) const CHUNK_BYTE_LIMIT: usize = 1_048_576;

/// `HarbormasterLogWorker`, which a closed log queues.
pub(super) const LOG_WORKER: &str = "HarbormasterLogWorker";

/// The longest log source `newLog()` keeps unchanged
/// (`PhutilUTF8StringTruncator` with 250 bytes).
pub(super) const SOURCE_LIMIT: usize = 250;

/// A log, once written and closed.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Log {
    /// `byteLength`.
    pub(super) byte_length: usize,
    /// `lineMap`, as Lisk stores it.
    pub(super) line_map: String,
    /// The chunks: `(headOffset, data)`.
    pub(super) chunks: Vec<(usize, Vec<u8>)>,
}

/// The line map as `updateLineMap()` keeps it: markers, the bytes and
/// lines since the last one, and the bytes held back for the next append.
#[derive(Debug, Clone, Default)]
struct LineMap {
    started: bool,
    markers: Vec<(usize, usize)>,
    map_bytes: usize,
    line_count: usize,
    prefix: Vec<u8>,
}

impl LineMap {
    /// `updateLineMap($append_data)` with the default marker distance.
    fn update(&mut self, data: &[u8]) {
        self.started = true;
        let marker_distance = CHUNK_BYTE_LIMIT / 2;
        let max_utf8_width = 8;
        let mut buffer = std::mem::take(&mut self.prefix);
        buffer.extend_from_slice(data);
        let (mut last_marker, mut last_count) = self.markers.last().copied().unwrap_or((0, 0));
        let mut pos = 0;
        let len = buffer.len();
        loop {
            if len - pos <= max_utf8_width * 2 {
                self.prefix = buffer[pos..].to_vec();
                break;
            }
            let mut slice_length = (marker_distance - self.map_bytes).min(len - pos - 1);
            let min_width = slice_length.saturating_sub(max_utf8_width).max(1);
            while slice_length >= min_width {
                let here = buffer[pos + slice_length - 1];
                let next = buffer[pos + slice_length];
                if here == b'\r' {
                    if next == b'\n' {
                        slice_length += 1;
                    }
                    break;
                }
                if next <= 0x7f || (0xc2..=0xf4).contains(&next) {
                    break;
                }
                slice_length -= 1;
            }
            let slice = &buffer[pos..pos + slice_length];
            pos += slice_length;
            self.map_bytes += slice_length;
            let crlf = count(slice, b"\r\n");
            let cr = count(slice, b"\r");
            let lf = count(slice, b"\n");
            self.line_count += crlf + (cr - crlf) + (lf - crlf);
            if self.map_bytes >= marker_distance - max_utf8_width {
                self.markers
                    .push((last_marker + self.map_bytes, last_count + self.line_count));
                last_count += self.line_count;
                self.line_count = 0;
                last_marker += self.map_bytes;
                self.map_bytes = 0;
            }
        }
    }

    /// The `lineMap` column: `[]` until something was appended.
    fn encode(&self) -> String {
        if !self.started {
            return "[]".to_owned();
        }
        let markers: Vec<Value> = self
            .markers
            .iter()
            .map(|(offset, lines)| json!([offset, lines]))
            .collect();
        let prefix = base64::engine::general_purpose::STANDARD.encode(&self.prefix);
        phab_core::php::json_encode(&json!([markers, self.map_bytes, self.line_count, prefix]))
    }
}

/// `substr_count()`.
fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

/// The log `append($text)` writes to a new log: chunks of at most
/// `CHUNK_BYTE_LIMIT` bytes, the line map updated with each.
pub(super) fn write(text: &[u8]) -> Log {
    let mut map = LineMap::default();
    let mut chunks = Vec::new();
    let mut offset = 0;
    for piece in text.chunks(CHUNK_BYTE_LIMIT) {
        chunks.push((offset, piece.to_vec()));
        map.update(piece);
        offset += piece.len();
    }
    Log {
        byte_length: offset,
        line_map: map.encode(),
        chunks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_line_map_like_php() {
        let text = "Declining to make service call because `phabricator.silent` is enabled in \
                    configuration.";
        let log = write(text.as_bytes());
        assert_eq!(log.byte_length, text.len());
        assert_eq!(log.chunks, vec![(0, text.as_bytes().to_vec())]);
        // Everything but the last byte is counted, which is held back.
        assert_eq!(log.line_map, format!("[[],{},0,\"Lg==\"]", text.len() - 1));
    }

    #[test]
    fn counts_lines_and_holds_back_short_texts() {
        let log = write(b"HTTP 200\nServer: x\r\nA: b\n");
        assert_eq!(log.line_map, "[[],24,2,\"Cg==\"]");
        let short = write(b"HTTP 7");
        assert_eq!(short.line_map, "[[],0,0,\"SFRUUCA3\"]");
        let empty = write(b"");
        assert_eq!(empty.line_map, "[]");
        assert_eq!(empty.chunks.len(), 0);
    }

    #[test]
    fn splits_chunks_at_the_limit() {
        let text = vec![b'a'; CHUNK_BYTE_LIMIT + 10];
        let log = write(&text);
        assert_eq!(log.chunks.len(), 2);
        assert_eq!(log.chunks[1].0, CHUNK_BYTE_LIMIT);
        assert!(
            log.line_map.starts_with("[[[524288,0],[1048575,0]]"),
            "{}",
            log.line_map
        );
    }
}
