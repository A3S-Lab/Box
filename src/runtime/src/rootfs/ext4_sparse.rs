//! Sparse host-file adaptation for the deterministic ext4 writer.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use a3s_box_core::error::{BoxError, Result};
use mkext4::InodeHandle;

pub(crate) enum FileFill {
    Dense {
        handle: InodeHandle,
        path: PathBuf,
    },
    Sparse {
        handle: InodeHandle,
        path: PathBuf,
        ranges: Vec<(u64, u64)>,
    },
}

impl FileFill {
    pub(crate) fn write_into<S: mkext4::RegionSink>(
        self,
        writer: &mut mkext4::build::ImageWriter<'_, S>,
    ) -> Result<()> {
        match self {
            Self::Dense { handle, path } => {
                let mut file = File::open(&path).map_err(BoxError::IoError)?;
                writer.fill(handle, &mut file).map_err(|error| {
                    BoxError::BuildError(format!(
                        "Failed to write {} into ext4 artifact: {error}",
                        path.display()
                    ))
                })
            }
            Self::Sparse {
                handle,
                path,
                ranges,
            } => {
                let file = File::open(&path).map_err(BoxError::IoError)?;
                let mut reader = SparseDataReader::new(file, ranges);
                writer.fill(handle, &mut reader).map_err(|error| {
                    BoxError::BuildError(format!(
                        "Failed to write sparse file {} into ext4 artifact: {error}",
                        path.display()
                    ))
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SourceSegment {
    Data { len: u64 },
    Hole { len: u64 },
}

pub(crate) struct SparseLayout {
    pub(crate) segments: Vec<SourceSegment>,
    pub(crate) data_ranges: Vec<(u64, u64)>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn sparse_layout(file: &File, length: u64) -> Result<Option<SparseLayout>> {
    if length == 0 {
        return Ok(None);
    }
    let mut segments = Vec::new();
    let mut data_ranges = Vec::new();
    let mut cursor = 0u64;
    let mut saw_hole = false;
    while cursor < length {
        let data = match seek_extent(file, cursor, libc::SEEK_DATA) {
            Ok(offset) => offset.min(length),
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => length,
            Err(error) if sparse_seek_unsupported(&error) => return Ok(None),
            Err(error) => return Err(BoxError::IoError(error)),
        };
        if data > cursor {
            segments.push(SourceSegment::Hole { len: data - cursor });
            saw_hole = true;
        }
        if data == length {
            break;
        }
        if data < cursor {
            return Err(BoxError::BuildError(
                "SEEK_DATA returned a regressed offset".to_string(),
            ));
        }
        let hole = match seek_extent(file, data, libc::SEEK_HOLE) {
            Ok(offset) => offset.min(length),
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => length,
            Err(error) if sparse_seek_unsupported(&error) => return Ok(None),
            Err(error) => return Err(BoxError::IoError(error)),
        };
        if hole <= data {
            return Err(BoxError::BuildError(
                "SEEK_HOLE did not advance past data".to_string(),
            ));
        }
        let len = hole - data;
        segments.push(SourceSegment::Data { len });
        data_ranges.push((data, len));
        cursor = hole;
    }

    if !saw_hole || !sparse_segments_supported_by_writer(&segments) {
        return Ok(None);
    }
    Ok(Some(SparseLayout {
        segments,
        data_ranges,
    }))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn sparse_layout(_file: &File, _length: u64) -> Result<Option<SparseLayout>> {
    Ok(None)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn seek_extent(file: &File, offset: u64, whence: libc::c_int) -> std::io::Result<u64> {
    let offset = libc::off_t::try_from(offset).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "file offset exceeds host off_t",
        )
    })?;
    // SAFETY: the file descriptor is valid for the call and lseek does not
    // outlive it. The returned offset is checked before conversion.
    let result = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(result as u64)
    }
}

fn sparse_seek_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL) | Some(libc::ENOTSUP) | Some(libc::ENOSYS)
    )
}

fn sparse_segments_supported_by_writer(segments: &[SourceSegment]) -> bool {
    segments.iter().enumerate().all(|(index, segment)| {
        let len = match segment {
            SourceSegment::Data { len } | SourceSegment::Hole { len } => *len,
        };
        index + 1 == segments.len() || len & 4095 == 0
    })
}

struct SparseDataReader {
    file: File,
    ranges: Vec<(u64, u64)>,
    range_index: usize,
    range_position: u64,
}

impl SparseDataReader {
    fn new(file: File, ranges: Vec<(u64, u64)>) -> Self {
        Self {
            file,
            ranges,
            range_index: 0,
            range_position: 0,
        }
    }
}

impl Read for SparseDataReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(&(offset, length)) = self.ranges.get(self.range_index) else {
                return Ok(0);
            };
            if self.range_position == length {
                self.range_index += 1;
                self.range_position = 0;
                continue;
            }
            self.file
                .seek(SeekFrom::Start(offset + self.range_position))?;
            let remaining = length - self.range_position;
            let requested = buffer.len().min(remaining as usize);
            let read = self
                .file
                .by_ref()
                .take(remaining)
                .read(&mut buffer[..requested])?;
            self.range_position += read as u64;
            return Ok(read);
        }
    }
}
