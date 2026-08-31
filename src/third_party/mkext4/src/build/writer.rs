//! Phase 3: emission. The metadata pass happens in `ImageWriter::new`
//! (every non-file-data byte, ascending, exactly once); `fill` streams
//! file contents; `finish` enforces completeness.

use super::seal::{Layout, SegSrc};
use super::InodeHandle;
use crate::sink::RegionSink;
use crate::spec::BLOCK_SIZE;
use crate::{Error, Result};
use std::io::Read;

/// Result of a completed image build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Total image length in bytes.
    pub image_len: u64,
    /// File bytes streamed through `fill`.
    pub data_bytes: u64,
}

enum FillState {
    /// Not a fillable file (dir, dropped, zero-length already complete).
    NotFillable,
    Pending,
    Done,
}

/// Phase-3 writer. Created by [`Layout::writer`].
pub struct ImageWriter<'a, S: RegionSink> {
    layout: &'a Layout,
    sink: S,
    fill_state: Vec<FillState>,
    pending: usize,
    data_bytes: u64,
    poisoned: bool,
    /// Reused across fills (one allocation per writer, not per file).
    /// 4 MiB: big enough that syscall count, not chunking, bounds large
    /// fills; small enough to be irrelevant per writer.
    scratch: Vec<u8>,
}

impl<'a, S: RegionSink> ImageWriter<'a, S> {
    pub(super) fn new(layout: &'a Layout, mut sink: S) -> Result<ImageWriter<'a, S>> {
        emit_metadata(layout, &mut sink)?;
        let mut pending = 0;
        let fill_state = layout
            .file_runs
            .iter()
            .enumerate()
            .map(|(slot, (runs, _size))| {
                // Dropped slots, non-files, and zero-length files (no
                // runs) have nothing to fill.
                if layout.slot_ino[slot] == 0 || runs.is_empty() {
                    FillState::NotFillable
                } else {
                    pending += 1;
                    FillState::Pending
                }
            })
            .collect();
        Ok(ImageWriter {
            layout,
            sink,
            fill_state,
            pending,
            data_bytes: 0,
            poisoned: false,
            scratch: vec![0u8; 1024 * BLOCK_SIZE],
        })
    }

    fn check_poisoned(&self) -> Result<()> {
        if self.poisoned {
            return Err(Error::Invalid(
                "writer is poisoned by an earlier failed fill; rebuild from the layout".into(),
            ));
        }
        Ok(())
    }

    /// Stream exactly the declared byte count for `f` from `reader`.
    /// Emits at the file's final offsets; ascending when files are filled
    /// in declaration order. A failure poisons the writer.
    pub fn fill(&mut self, f: InodeHandle, reader: &mut impl Read) -> Result<()> {
        self.check_poisoned()?;
        let slot = f.0 as usize;
        match self.fill_state.get(slot) {
            Some(FillState::Pending) => {}
            Some(FillState::Done) => return Err(Error::Invalid("file already filled".into())),
            _ => {
                return Err(Error::Invalid(
                    "handle is not a fillable file in this layout".into(),
                ))
            }
        }
        match self.fill_inner(slot, reader) {
            Ok(()) => {
                self.fill_state[slot] = FillState::Done;
                self.pending -= 1;
                Ok(())
            }
            Err(e) => {
                self.poisoned = true;
                Err(e)
            }
        }
    }

    fn fill_inner(&mut self, slot: usize, reader: &mut impl Read) -> Result<()> {
        let (runs, size) = &self.layout.file_runs[slot];
        let mut remaining = *size;
        let buf = &mut self.scratch;
        for run in runs {
            let mut offset = run.start * BLOCK_SIZE as u64;
            let mut run_bytes = (run.len * BLOCK_SIZE as u64).min(remaining);
            // The final block of the file may be partial: emit content
            // then zero padding to the block boundary.
            while run_bytes > 0 {
                let want = run_bytes.min(buf.len() as u64) as usize;
                reader
                    .read_exact(&mut buf[..want])
                    .map_err(|e| Error::Io(std::io::Error::other(format!("short fill: {e}"))))?;
                self.sink.data(offset, &buf[..want])?;
                offset += want as u64;
                run_bytes -= want as u64;
                remaining -= want as u64;
            }
            let pad = (BLOCK_SIZE as u64 - offset % BLOCK_SIZE as u64) % BLOCK_SIZE as u64;
            if pad > 0 {
                self.sink.zeros(offset, pad)?;
            }
        }
        self.data_bytes += *size;
        Ok(())
    }

    /// Complete the image. Errors if any declared file went unfilled or
    /// the writer is poisoned.
    pub fn finish(self) -> Result<Summary> {
        self.check_poisoned()?;
        if self.pending > 0 {
            return Err(Error::Invalid(format!(
                "{} declared file(s) never filled",
                self.pending
            )));
        }
        Ok(Summary {
            image_len: self.layout.image_len(),
            data_bytes: self.data_bytes,
        })
    }
}

/// The metadata pass: one ascending sweep emitting every block that is
/// not declared file data — metadata segments as `data`, everything else
/// as `zeros`.
fn emit_metadata<S: RegionSink>(layout: &Layout, sink: &mut S) -> Result<()> {
    let bs = BLOCK_SIZE as u64;
    let total_blocks = layout.image_len() / bs;
    let mut cursor = 0u64; // block
    let mut data_iter = layout.data_runs.iter().peekable();

    let emit_gap =
        |sink: &mut S,
         from: u64,
         to: u64,
         data_iter: &mut std::iter::Peekable<std::slice::Iter<crate::layout::alloc::Run>>|
         -> Result<()> {
            // [from, to) holds no metadata; zero everything except file data.
            let mut at = from;
            while at < to {
                // Skip data runs that start before `at`.
                while let Some(r) = data_iter.peek() {
                    if r.start + r.len <= at {
                        data_iter.next();
                    } else {
                        break;
                    }
                }
                match data_iter.peek() {
                    Some(r) if r.start < to => {
                        if r.start > at {
                            sink.zeros(at * bs, (r.start - at) * bs)?;
                        }
                        at = (r.start + r.len).min(to);
                        if r.start + r.len <= to {
                            data_iter.next();
                        }
                    }
                    _ => {
                        sink.zeros(at * bs, (to - at) * bs)?;
                        at = to;
                    }
                }
            }
            Ok(())
        };

    for seg in &layout.segments {
        if seg.block > cursor {
            emit_gap(sink, cursor, seg.block, &mut data_iter)?;
        }
        match &seg.src {
            SegSrc::Bytes(bytes) => {
                debug_assert_eq!(bytes.len() as u64, seg.len * bs);
                sink.data(seg.block * bs, bytes)?;
            }
            SegSrc::Itable { group } => {
                emit_itable(layout, *group, seg.block, seg.len, sink)?;
            }
        }
        cursor = seg.block + seg.len;
    }
    if cursor < total_blocks {
        emit_gap(sink, cursor, total_blocks, &mut data_iter)?;
    }
    Ok(())
}

/// Render one group's inode table block-by-block (16 inodes per block).
/// Inode numbering is dense from 1, so the used prefix of the table is a
/// contiguous slice of the rendered-inode array; the rest is one zeros
/// run.
fn emit_itable<S: RegionSink>(
    layout: &Layout,
    group: u32,
    start_block: u64,
    len: u64,
    sink: &mut S,
) -> Result<()> {
    let ipg = layout.geo.inodes_per_group;
    let first_ino = group * ipg + 1;
    let bs = BLOCK_SIZE as u64;
    let last_used_in_group = layout
        .max_ino
        .clamp(first_ino.saturating_sub(1), first_ino + ipg - 1)
        .saturating_sub(first_ino - 1);
    let used_blocks = (last_used_in_group as u64).div_ceil(16);
    let mut buf = vec![0u8; BLOCK_SIZE];
    for blk in 0..used_blocks {
        let base = (first_ino - 1) as usize + blk as usize * 16;
        let avail = layout.inodes.len().saturating_sub(base).min(16);
        for (i, raw) in layout.inodes[base..base + avail].iter().enumerate() {
            buf[i * 256..(i + 1) * 256].copy_from_slice(raw);
        }
        buf[avail * 256..].fill(0);
        sink.data((start_block + blk) * bs, &buf)?;
    }
    if used_blocks < len {
        sink.zeros((start_block + used_blocks) * bs, (len - used_blocks) * bs)?;
    }
    Ok(())
}
