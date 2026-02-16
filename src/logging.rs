use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::fmt::writer::MakeWriter;

const DEFAULT_MAX_LOG_LINES: usize = 1000;

static GLOBAL_LOG_BUFFER: OnceLock<LogBuffer> = OnceLock::new();

#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<String>>>,
    max_lines: usize,
}

impl LogBuffer {
    fn new(max_lines: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(max_lines))),
            max_lines,
        }
    }

    fn append_bytes(&self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        for line in text.split('\n') {
            let normalized = line.trim_end_matches('\r');
            if normalized.is_empty() {
                continue;
            }
            let cleaned = strip_ansi_escape_sequences(normalized);
            let cleaned = cleaned.trim();
            if cleaned.is_empty() {
                continue;
            }
            self.push_line(cleaned.to_string());
        }
    }

    fn push_line(&self, line: String) {
        if let Ok(mut guard) = self.inner.lock() {
            while guard.len() >= self.max_lines {
                let _ = guard.pop_front();
            }
            guard.push_back(line);
        }
    }

    pub fn recent_lines(&self, limit: usize) -> Vec<String> {
        if let Ok(guard) = self.inner.lock() {
            let start = guard.len().saturating_sub(limit);
            return guard.iter().skip(start).cloned().collect();
        }
        Vec::new()
    }
}

pub fn init_global_log_buffer(max_lines: usize) -> LogBuffer {
    let max_lines = if max_lines == 0 {
        DEFAULT_MAX_LOG_LINES
    } else {
        max_lines
    };
    GLOBAL_LOG_BUFFER
        .get_or_init(|| LogBuffer::new(max_lines))
        .clone()
}

pub fn global_log_buffer() -> Option<LogBuffer> {
    GLOBAL_LOG_BUFFER.get().cloned()
}

#[derive(Clone)]
pub struct TeeMakeWriter {
    logs: LogBuffer,
}

impl TeeMakeWriter {
    pub fn new(logs: LogBuffer) -> Self {
        Self { logs }
    }
}

pub struct TeeWriter {
    logs: LogBuffer,
    stderr: io::Stderr,
}

impl<'a> MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            logs: self.logs.clone(),
            stderr: io::stderr(),
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.stderr.write(buf)?;
        self.logs.append_bytes(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()
    }
}

fn strip_ansi_escape_sequences(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\u{1b}' {
            if !c.is_control() || c == '\t' {
                output.push(c);
            }
            i += 1;
            continue;
        }

        i += 1;
        if i >= chars.len() {
            break;
        }

        match chars[i] {
            '[' => {
                i += 1;
                while i < chars.len() {
                    let terminal = chars[i];
                    if ('@'..='~').contains(&terminal) {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            ']' => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\u{0007}' {
                        i += 1;
                        break;
                    }
                    if chars[i] == '\u{1b}' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            'P' | 'X' | '^' | '_' => {
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\u{1b}' && i + 1 < chars.len() && chars[i + 1] == '\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::LogBuffer;

    #[test]
    fn retains_most_recent_lines() {
        let logs = LogBuffer::new(3);
        logs.append_bytes(b"one\ntwo\n");
        logs.append_bytes(b"three\nfour\n");

        assert_eq!(logs.recent_lines(10), vec!["two", "three", "four"]);
        assert_eq!(logs.recent_lines(2), vec!["three", "four"]);
    }

    #[test]
    fn strips_ansi_escape_sequences() {
        let logs = LogBuffer::new(10);
        logs.append_bytes(b"\x1b[2m2026-02-16\x1b[0m \x1b[32mINFO\x1b[0m running\n");
        logs.append_bytes(b"\x1b]8;;https://example.com\x1b\\label\x1b]8;;\x1b\\\n");

        assert_eq!(
            logs.recent_lines(10),
            vec!["2026-02-16 INFO running", "label"]
        );
    }
}
