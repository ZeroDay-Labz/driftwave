//! A byte buffer that is filled by a download thread while a decoder reads
//! from it, so playback can start before the download finishes.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};

#[derive(Default)]
struct Buf {
    data: Vec<u8>,
    /// Final size, when known up front (Content-Length).
    total: Option<u64>,
    done: bool,
    error: Option<String>,
    /// Set when nobody needs the data any more; the downloader stops.
    cancelled: bool,
}

#[derive(Default)]
pub struct StreamBuffer {
    buf: Mutex<Buf>,
    grew: Condvar,
}

impl StreamBuffer {
    pub fn new(total: Option<u64>) -> Arc<Self> {
        let s = Self::default();
        s.buf.lock().unwrap().total = total;
        Arc::new(s)
    }

    /// Returns false if the reader side has gone away and downloading
    /// should stop.
    pub fn append(&self, bytes: &[u8]) -> bool {
        let mut b = self.buf.lock().unwrap();
        b.data.extend_from_slice(bytes);
        self.grew.notify_all();
        !b.cancelled
    }

    pub fn finish(&self, error: Option<String>) {
        let mut b = self.buf.lock().unwrap();
        b.done = true;
        b.error = error;
        if b.total.is_none() {
            b.total = Some(b.data.len() as u64);
        }
        self.grew.notify_all();
    }

    pub fn cancel(&self) {
        let mut b = self.buf.lock().unwrap();
        b.cancelled = true;
        self.grew.notify_all();
    }

    /// (bytes downloaded, total if known, finished)
    pub fn progress(&self) -> (u64, Option<u64>, bool) {
        let b = self.buf.lock().unwrap();
        (b.data.len() as u64, b.total, b.done)
    }


    pub fn reader(self: &Arc<Self>) -> StreamReader {
        StreamReader { buf: self.clone(), pos: 0 }
    }
}

/// `Read + Seek` view of a `StreamBuffer`; reads block until the bytes
/// have been downloaded.
pub struct StreamReader {
    buf: Arc<StreamBuffer>,
    pos: u64,
}

impl StreamReader {
    /// Total length, blocking until it is known.
    fn total(&self) -> u64 {
        let mut b = self.buf.buf.lock().unwrap();
        loop {
            if let Some(t) = b.total {
                return t;
            }
            if b.cancelled {
                return b.data.len() as u64;
            }
            b = self.buf.grew.wait(b).unwrap();
        }
    }
}

impl Read for StreamReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut b = self.buf.buf.lock().unwrap();
        while self.pos >= b.data.len() as u64 && !b.done && !b.cancelled {
            b = self.buf.grew.wait(b).unwrap();
        }
        let avail = b.data.len() as u64;
        if self.pos >= avail {
            return match &b.error {
                Some(e) => Err(io::Error::other(e.clone())),
                None => Ok(0), // EOF
            };
        }
        let start = self.pos as usize;
        let n = out.len().min(avail as usize - start);
        out[..n].copy_from_slice(&b.data[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.total() as i64 + d,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn reads_block_until_data_arrives() {
        let buf = StreamBuffer::new(None);
        let mut r = buf.reader();
        let writer = {
            let buf = buf.clone();
            thread::spawn(move || {
                for chunk in [b"hello ".as_slice(), b"streaming ", b"world"] {
                    thread::sleep(Duration::from_millis(20));
                    buf.append(chunk);
                }
                buf.finish(None);
            })
        };
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello streaming world");
        writer.join().unwrap();

        // Seeking from the end works once the length is known.
        r.seek(SeekFrom::End(-5)).unwrap();
        let mut tail = String::new();
        r.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "world");
    }

    #[test]
    fn download_error_surfaces_at_the_gap() {
        let buf = StreamBuffer::new(None);
        buf.append(b"abc");
        buf.finish(Some("connection reset".into()));
        let mut r = buf.reader();
        let mut three = [0; 3];
        r.read_exact(&mut three).unwrap();
        assert!(r.read(&mut [0; 1]).is_err());
    }
}
