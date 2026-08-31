use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use a3s_box_core::error::{BoxError, Result};

use super::super::ext4::sparse::{SourceSegment, SparseLayout};
use super::tree::{build_error, LogicalRootfs};

impl LogicalRootfs {
    pub(super) fn spool<R: Read>(
        &mut self,
        reader: &mut R,
        expected_size: u64,
    ) -> Result<(PathBuf, Option<SparseLayout>)> {
        self.spool_sequence = self
            .spool_sequence
            .checked_add(1)
            .ok_or_else(|| build_error("OCI content spool sequence overflow"))?;
        let path = self
            .spool
            .path()
            .join(format!("content-{:016x}", self.spool_sequence));
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(BoxError::IoError)?;
        let read_limit = expected_size
            .checked_add(1)
            .ok_or_else(|| build_error("OCI entry size exceeds the supported range"))?;
        let mut input = reader.take(read_limit);
        let mut copied = 0u64;
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut segments = Vec::new();
        let mut data_ranges: Vec<(u64, u64)> = Vec::new();
        let mut saw_hole = false;
        loop {
            let mut filled = 0;
            while filled < buffer.len() {
                let read = input
                    .read(&mut buffer[filled..])
                    .map_err(BoxError::IoError)?;
                if read == 0 {
                    break;
                }
                filled += read;
            }
            if filled == 0 {
                break;
            }
            let offset = copied;
            copied = copied
                .checked_add(filled as u64)
                .ok_or_else(|| build_error("OCI entry byte count overflow"))?;
            if copied > expected_size {
                return Err(build_error(format!(
                    "OCI entry yielded more than its declared {expected_size} bytes"
                )));
            }
            if buffer[..filled].iter().all(|byte| *byte == 0) {
                push_segment(&mut segments, false, filled as u64);
                saw_hole = true;
                output
                    .seek(SeekFrom::Current(filled as i64))
                    .map_err(BoxError::IoError)?;
            } else {
                push_segment(&mut segments, true, filled as u64);
                if let Some((range_offset, range_len)) = data_ranges.last_mut() {
                    if range_offset.saturating_add(*range_len) == offset {
                        *range_len = range_len.saturating_add(filled as u64);
                    } else {
                        data_ranges.push((offset, filled as u64));
                    }
                } else {
                    data_ranges.push((offset, filled as u64));
                }
                output
                    .write_all(&buffer[..filled])
                    .map_err(BoxError::IoError)?;
            }
        }
        if copied != expected_size {
            return Err(build_error(format!(
                "OCI entry yielded {copied} bytes instead of {expected_size}"
            )));
        }
        output.set_len(expected_size).map_err(BoxError::IoError)?;
        output.flush().map_err(BoxError::IoError)?;
        let sparse = saw_hole.then_some(SparseLayout {
            segments,
            data_ranges,
        });
        Ok((path, sparse))
    }
}

fn push_segment(segments: &mut Vec<SourceSegment>, data: bool, len: u64) {
    match segments.last_mut() {
        Some(SourceSegment::Data { len: previous }) if data => {
            *previous = previous.saturating_add(len)
        }
        Some(SourceSegment::Hole { len: previous }) if !data => {
            *previous = previous.saturating_add(len)
        }
        _ if data => segments.push(SourceSegment::Data { len }),
        _ => segments.push(SourceSegment::Hole { len }),
    }
}
