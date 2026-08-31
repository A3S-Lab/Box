//! The streaming output contract.
//!
//! A [`RegionSink`] receives every byte of the image exactly once, final
//! when emitted, as positioned writes: explicit bytes via `data`,
//! untouched space via `zeros` (so consumers can retire zero regions
//! without any I/O).

use std::io;

/// Where the image bytes go. Deliberately dumb: positioned writes only,
/// no seeks, no rewrites.
///
/// Contract (upheld by the writer, checkable with [`CheckingSink`]):
/// every byte of `[0, image_len)` is covered exactly once across the
/// writer's lifetime, by `data` or `zeros`, and is final when emitted.
pub trait RegionSink {
    /// `bytes` are the final content at `offset`.
    fn data(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
    /// `len` zero bytes at `offset` (no buffer — consumers can elide).
    fn zeros(&mut self, offset: u64, len: u64) -> io::Result<()>;
}

impl<S: RegionSink + ?Sized> RegionSink for &mut S {
    fn data(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        (**self).data(offset, bytes)
    }
    fn zeros(&mut self, offset: u64, len: u64) -> io::Result<()> {
        (**self).zeros(offset, len)
    }
}

/// Materializes the image in memory. For tests and small images.
#[derive(Debug, Default)]
pub struct VecSink {
    /// The image bytes (grows as regions arrive).
    pub buf: Vec<u8>,
}

impl RegionSink for VecSink {
    fn data(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let end = offset as usize + bytes.len();
        if self.buf.len() < end {
            self.buf.resize(end, 0);
        }
        self.buf[offset as usize..end].copy_from_slice(bytes);
        Ok(())
    }
    fn zeros(&mut self, offset: u64, len: u64) -> io::Result<()> {
        let end = (offset + len) as usize;
        if self.buf.len() < end {
            self.buf.resize(end, 0);
        }
        // Already zero on resize; explicit for the overwrite case (which
        // CheckingSink would reject anyway).
        self.buf[offset as usize..end].fill(0);
        Ok(())
    }
}

/// Writes the image to a file with positioned writes; `zeros` regions
/// are elided (the file is extended sparsely instead), so untouched
/// space costs no I/O.
#[cfg(unix)]
#[derive(Debug)]
pub struct FileSink {
    file: std::fs::File,
    len: u64,
}

#[cfg(unix)]
impl FileSink {
    /// Create (truncate) `path` and pre-size it to `image_len` — the
    /// kernel materializes the zeros lazily.
    pub fn create(path: &std::path::Path, image_len: u64) -> io::Result<FileSink> {
        let file = std::fs::File::create(path)?;
        file.set_len(image_len)?;
        Ok(FileSink {
            file,
            len: image_len,
        })
    }

    /// Return the file (nothing is buffered; call `sync_all` yourself if
    /// you need durability guarantees).
    pub fn into_file(self) -> std::fs::File {
        self.file
    }
}

#[cfg(unix)]
impl RegionSink for FileSink {
    fn data(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        std::os::unix::fs::FileExt::write_all_at(&self.file, bytes, offset)
    }
    fn zeros(&mut self, offset: u64, len: u64) -> io::Result<()> {
        // Already zero via set_len; validate the range instead.
        if offset + len > self.len {
            return Err(io::Error::other("zeros past image end"));
        }
        Ok(())
    }
}

/// Wraps a sink and asserts the exactly-once coverage contract; used by
/// the test suite around every image build.
#[derive(Debug)]
pub struct CheckingSink<S> {
    inner: S,
    /// Sorted, disjoint covered ranges.
    covered: Vec<(u64, u64)>,
}

impl<S: RegionSink> CheckingSink<S> {
    /// Wrap `inner`.
    pub fn new(inner: S) -> Self {
        CheckingSink {
            inner,
            covered: Vec::new(),
        }
    }

    fn record(&mut self, offset: u64, len: u64) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = offset + len;
        // Binary search for the insertion point, then check neighbors.
        let i = self.covered.partition_point(|&(s, _)| s < offset);
        if i > 0 {
            let (ps, pe) = self.covered[i - 1];
            if pe > offset {
                return Err(io::Error::other(format!(
                    "overlap: [{offset:#x},{end:#x}) with [{ps:#x},{pe:#x})"
                )));
            }
        }
        if i < self.covered.len() {
            let (ns, ne) = self.covered[i];
            if ns < end {
                return Err(io::Error::other(format!(
                    "overlap: [{offset:#x},{end:#x}) with [{ns:#x},{ne:#x})"
                )));
            }
        }
        // Coalesce with neighbors to keep the list small.
        let merge_prev = i > 0 && self.covered[i - 1].1 == offset;
        let merge_next = i < self.covered.len() && self.covered[i].0 == end;
        match (merge_prev, merge_next) {
            (true, true) => {
                self.covered[i - 1].1 = self.covered[i].1;
                self.covered.remove(i);
            }
            (true, false) => self.covered[i - 1].1 = end,
            (false, true) => self.covered[i].0 = offset,
            (false, false) => self.covered.insert(i, (offset, end)),
        }
        Ok(())
    }

    /// Assert that exactly `[0, image_len)` was covered, then return the
    /// inner sink.
    pub fn finish(self, image_len: u64) -> io::Result<S> {
        if self.covered != [(0, image_len)] {
            return Err(io::Error::other(format!(
                "coverage is not exactly [0, {image_len:#x}): {:x?}",
                &self.covered[..self.covered.len().min(8)]
            )));
        }
        Ok(self.inner)
    }

    /// Access the wrapped sink.
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S: RegionSink> RegionSink for CheckingSink<S> {
    fn data(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.record(offset, bytes.len() as u64)?;
        self.inner.data(offset, bytes)
    }
    fn zeros(&mut self, offset: u64, len: u64) -> io::Result<()> {
        self.record(offset, len)?;
        self.inner.zeros(offset, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checking_sink_accepts_exact_tiling() {
        let mut s = CheckingSink::new(VecSink::default());
        s.data(0, &[1, 2, 3]).unwrap();
        s.zeros(3, 5).unwrap();
        s.data(8, &[9]).unwrap();
        let inner = s.finish(9).unwrap();
        assert_eq!(inner.buf, [1, 2, 3, 0, 0, 0, 0, 0, 9]);
    }

    #[test]
    fn checking_sink_rejects_overlap() {
        let mut s = CheckingSink::new(VecSink::default());
        s.data(0, &[0; 8]).unwrap();
        assert!(s.zeros(4, 8).is_err());
    }

    #[test]
    fn checking_sink_rejects_gap() {
        let mut s = CheckingSink::new(VecSink::default());
        s.data(0, &[0; 4]).unwrap();
        s.data(8, &[0; 4]).unwrap();
        assert!(s.finish(12).is_err());
    }

    #[test]
    fn checking_sink_out_of_order_ok() {
        let mut s = CheckingSink::new(VecSink::default());
        s.data(8, &[0; 4]).unwrap();
        s.data(0, &[0; 8]).unwrap();
        s.finish(12).unwrap();
    }
}
