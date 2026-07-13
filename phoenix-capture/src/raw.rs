use std::os::windows::ffi::OsStrExt;

use phoenix_core::container::{Extent, CHUNK_SIZE};
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::manifest::ChunkRecord;
use phoenix_core::relocation::RelocationMap;
use phoenix_core::ProgressHandle;
use windows_sys::Win32::Foundation::GetLastError;

use crate::reader::BlockSource;

/// The knobs every restore path threads through unchanged: whether to
/// BLAKE3-check each chunk as it decompresses, where to report progress, and
/// how many bytes the partitions restored *before* this one already
/// contributed — progress is reported against the whole-disk total, so each
/// partition has to pick up the running count where the last one left off.
#[derive(Clone, Copy)]
pub struct RestoreOpts<'a> {
    pub verify: bool,
    pub progress: Option<&'a ProgressHandle>,
    pub bytes_done: u64,
}

/// How many bytes of a write at partition-relative `offset` of `len` bytes may
/// actually be written without reaching `max_bytes`. `None` = write nothing
/// (fully out of bounds); `Some(n)` = write the first `n` bytes.
fn clamp_write(offset: u64, len: usize, max_bytes: u64) -> Option<usize> {
    if offset >= max_bytes {
        return None;
    }
    let room = (max_bytes - offset) as usize;
    let n = room.min(len);
    (n > 0).then_some(n)
}

pub fn capture_raw(
    reader: &mut impl BlockSource,
    stream: &mut phoenix_core::container::PartitionStreamWriter<'_>,
) -> Result<()> {
    stream.set_extent(0);

    let mut pos = 0u64;
    let mut chunk_buf = vec![0u8; CHUNK_SIZE];
    while pos < reader.length() {
        let to_read = CHUNK_SIZE.min((reader.length() - pos) as usize);
        let n = reader.read_at(pos, &mut chunk_buf[..to_read])?;
        if n == 0 {
            // A raw partition should be fully readable; a 0-byte read before the
            // end means we'd drop the tail. Never silently produce a short image.
            return Err(PhoenixError::Other(format!(
                "capture_raw: read 0 bytes at offset {pos} (partition length {}). \
                 Refusing to write an incomplete backup.",
                reader.length(),
            )));
        }
        stream.write_chunk(&chunk_buf[..n])?;
        pos += n as u64;
    }
    Ok(())
}

pub fn finish_stream(
    stream: phoenix_core::container::PartitionStreamWriter<'_>,
) -> Result<Vec<ChunkRecord>> {
    stream.finish().map(|(records, _)| records)
}

/// Stream every chunk of `entry` from the open `.phnx` reader into the
/// target disk via `writer`. Returns the number of *bytes* written so the
/// caller can drive a byte-scaled progress meter — the GUI runs the
/// progress current/total through `format_bytes`, so reporting in chunks
/// would surface "1.57 KB / 5.29 KB" for a 21 GB partition (that was the
/// pre-fix behavior).
///
/// Position math, the bit that used to be wrong: each chunk's destination
/// offset is `extent.start_sector * sector_size + chunk.chunk_index *
/// CHUNK_SIZE`. The previous code dropped the second term, so every chunk
/// in an extent overwrote the same 4 MiB region — only the last chunk per
/// extent survived, and a "successful" restore produced a torched volume.
/// `chunk.chunk_index` is the within-extent ordinal that the capture path
/// stamps in `PartitionStreamWriter::write_chunk` (resets to 0 on every
/// `set_extent`), so we just read it back here.
///
/// `relocation` is `Some` when the target is smaller than the source and
/// post-boundary clusters need to land at a translated offset (Phase B
/// shrink relocation). When `None`, every chunk goes to its source
/// offset and behavior is identical to the same-size restore path. When
/// `Some`, each chunk is split through `RelocationMap::translate_write`
/// so a chunk straddling the safe-zone boundary turns into two writes
/// (one identity-mapped, one relocated). Most chunks fall entirely
/// inside one entry and produce a single segment.
pub fn restore_raw(
    reader: &mut phoenix_core::container::PhnxReader,
    entry: &phoenix_core::container::PartitionIndexEntry,
    writer: &mut PartitionWriter,
    opts: RestoreOpts<'_>,
    relocation: Option<&RelocationMap>,
    // Never write at or past this partition-relative byte (the target
    // partition size). Data beyond it is dropped: for a raw-captured volume
    // restored into a smaller partition this trims trailing free space that
    // the boot-sector patch is about to declare out of bounds, and it's a
    // hard guard against a stream overrunning into the neighboring partition.
    max_bytes: u64,
) -> Result<u64> {
    let RestoreOpts {
        verify,
        progress,
        mut bytes_done,
    } = opts;
    let stream = reader.read_stream_header(entry)?;
    let chunk_records: Vec<phoenix_core::manifest::ChunkRecord> = reader
        .manifest
        .partitions
        .iter()
        .find(|p| p.index == entry.index)
        .map(|p| p.chunks.clone())
        .ok_or_else(|| PhoenixError::Manifest("missing partition".into()))?;

    let sector_size = entry.sector_size as u64;
    let mut bytes_for_partition = 0u64;

    // Parallel decode: a reader thread streams the compressed chunks out of
    // the .phnx in file order, a worker pool decompresses (and, when `verify`
    // is on, BLAKE3-checks) them, and the closure below consumes the
    // plaintext IN CHUNK ORDER on this thread — the raw-disk handle stays
    // here, and target writes keep their monotonically increasing offsets.
    let paired: Vec<_> =
        phoenix_core::container::paired_chunks(&stream.chunks, &chunk_records, entry.index)?
            .collect();
    let items: Vec<phoenix_core::pipeline::DecodeItem> = paired
        .iter()
        .map(|(chunk, record)| phoenix_core::pipeline::DecodeItem {
            file_offset: chunk.file_offset,
            compressed_len: chunk.compressed_len,
            uncompressed_len: chunk.uncompressed_len,
            expected_blake3: verify.then(|| record.blake3.clone()),
            partition_index: entry.index,
            chunk_index: record.chunk_index,
        })
        .collect();

    reader.process_chunks(&items, |seq, data| {
        let (chunk, record) = paired[seq];
        if let Some(p) = progress {
            if p.is_cancelled() {
                return Err(PhoenixError::Cancelled);
            }
        }
        let extent = stream
            .extents
            .get(chunk.extent_index as usize)
            .ok_or_else(|| {
                PhoenixError::InvalidFormat(format!(
                    "chunk references extent index {} but partition {} only has {} extent(s)",
                    chunk.extent_index,
                    entry.index,
                    stream.extents.len()
                ))
            })?;
        let src_offset =
            extent.start_sector * sector_size + (chunk.chunk_index as u64) * CHUNK_SIZE as u64;

        match relocation {
            None => {
                if let Some(clamped) = clamp_write(src_offset, data.len(), max_bytes) {
                    writer.write_at(src_offset, &data[..clamped])?;
                }
            }
            Some(map) => {
                // A chunk that fully lives below the safe boundary OR
                // fully inside one relocation entry produces a single
                // segment with len == data.len(). A chunk straddling
                // the boundary produces two. Splitting at the byte
                // level is the map's job; we just dispatch the writes.
                let segments = map.translate_write(src_offset, data.len());
                if segments.is_empty() {
                    return Err(PhoenixError::Other(format!(
                        "relocation map produced zero segments for chunk at src_offset={} \
                         len={} (this is a bug in build_relocation_map)",
                        src_offset,
                        data.len()
                    )));
                }
                for seg in segments {
                    if let Some(clamped) = clamp_write(seg.dst_byte_offset, seg.len, max_bytes) {
                        let end = seg.source_offset + clamped;
                        writer.write_at(seg.dst_byte_offset, &data[seg.source_offset..end])?;
                    }
                }
            }
        }
        bytes_done += data.len() as u64;
        bytes_for_partition += data.len() as u64;
        if let Some(p) = progress {
            p.set(
                bytes_done,
                format!("Chunk {} / {}", record.chunk_index + 1, chunk_records.len()),
            );
        }
        Ok(())
    })?;
    Ok(bytes_for_partition)
}

pub struct PartitionWriter {
    handle: windows_sys::Win32::Foundation::HANDLE,
    base_offset: u64,
    /// Logical sector size of the target disk (512 or 4096). Raw-disk
    /// handles reject `ReadFile`/`WriteFile` calls whose offset or length
    /// isn't a multiple of this, so `write_at` transparently read-modify-
    /// writes an aligned span when a caller (e.g. a 512-byte boot-sector
    /// patch on a 4Kn disk) hands us a sub-sector-aligned write.
    sector_size: u64,
}

impl PartitionWriter {
    pub fn open_disk(disk_path: &str, partition_offset: u64, sector_size: u32) -> Result<Self> {
        let wide: Vec<u16> = std::ffi::OsStr::new(disk_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            windows_sys::Win32::Storage::FileSystem::CreateFileW(
                wide.as_ptr(),
                0x4000_0000 | 0x8000_0000, // GENERIC_WRITE | GENERIC_READ
                windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE,
                std::ptr::null(),
                windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(phoenix_core::error::PhoenixError::Disk(
                "cannot open disk for write".into(),
            ));
        }
        Ok(Self {
            handle,
            base_offset: partition_offset,
            sector_size: (sector_size as u64).max(512),
        })
    }

    /// Sector-aligned read at a partition-relative offset. Used by the
    /// post-restore boot-sector patchers (NTFS / FAT32 / exFAT) to do a
    /// read-modify-write through the same handle that streamed the data,
    /// so we don't have to reopen `\\.\PhysicalDriveN` and risk an
    /// auto-mount race in between. Both `relative_offset` and `data.len()`
    /// must be sector-aligned: the handle is opened against a raw disk
    /// device, which only accepts sector-multiples for `ReadFile`.
    pub fn read_at(&mut self, relative_offset: u64, data: &mut [u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let abs = self.base_offset + relative_offset;
        let ss = self.sector_size;
        if abs.is_multiple_of(ss) && (data.len() as u64).is_multiple_of(ss) {
            return self.read_raw(abs, data);
        }
        // Sub-sector-aligned read (e.g. a 512-byte boot-sector read-back on
        // a 4Kn disk): read the enclosing aligned span and copy out the
        // requested slice. Mirrors the alignment bounce in `write_at`.
        let aligned_start = abs - (abs % ss);
        let aligned_end = (abs + data.len() as u64).div_ceil(ss) * ss;
        let span = (aligned_end - aligned_start) as usize;
        let mut buf = vec![0u8; span];
        self.read_raw(aligned_start, &mut buf)?;
        let inner = (abs - aligned_start) as usize;
        data.copy_from_slice(&buf[inner..inner + data.len()]);
        Ok(())
    }

    /// Absolute-offset raw read. Both `abs` and `data.len()` must be
    /// sector-aligned (the caller guarantees this — `read_at` is only
    /// invoked by the boot-sector patchers with sector-sized buffers, and
    /// the RMW path in `write_at` computes an aligned span).
    fn read_raw(&mut self, abs: u64, data: &mut [u8]) -> Result<()> {
        unsafe {
            let mut dist = 0i64;
            if windows_sys::Win32::Storage::FileSystem::SetFilePointerEx(
                self.handle,
                abs as i64,
                &mut dist,
                0,
            ) == 0
            {
                let err = GetLastError();
                return Err(PhoenixError::Disk(format!(
                    "SetFilePointerEx to disk offset {abs} for read failed (Win32 error {err})"
                )));
            }
            let mut read = 0u32;
            if windows_sys::Win32::Storage::FileSystem::ReadFile(
                self.handle,
                data.as_mut_ptr() as *mut _,
                data.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            ) == 0
            {
                let err = GetLastError();
                return Err(PhoenixError::Disk(format!(
                    "ReadFile of {} bytes at disk offset {abs} failed (Win32 error {err})",
                    data.len()
                )));
            }
            if read as usize != data.len() {
                return Err(PhoenixError::Disk(format!(
                    "ReadFile short read at disk offset {abs}: got {read} of {} bytes",
                    data.len()
                )));
            }
        }
        Ok(())
    }

    /// Best-effort `FlushFileBuffers` so any cached writes are committed
    /// before we go to read the boot sector back for patching. Without it,
    /// in principle a cached restore_raw write could still be in flight when
    /// the patcher reads sector 0 and the read would see stale bytes — in
    /// practice the raw-disk handle path is unbuffered, but the cost of
    /// being explicit here is one IOCTL and the safety win is real.
    pub fn flush(&mut self) -> Result<()> {
        unsafe {
            if windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(self.handle) == 0 {
                let err = GetLastError();
                return Err(PhoenixError::Disk(format!(
                    "FlushFileBuffers failed (Win32 error {err})"
                )));
            }
        }
        Ok(())
    }

    /// Sector-aligned write at a partition-relative offset. Errors carry
    /// both the absolute disk offset and the Win32 error code so the GUI
    /// status no longer collapses every failure to the unhelpful
    /// "write failed" string we used to emit. Common codes the user
    /// might see:
    ///   * 5 (ERROR_ACCESS_DENIED) — another process holds a handle on
    ///     the volume; usually a re-mount races us.
    ///   * 19 (ERROR_WRITE_PROTECT) — disk is read-only.
    ///   * 87 (ERROR_INVALID_PARAMETER) — offset/length not aligned to
    ///     the disk's logical sector size.
    ///   * 33 (ERROR_LOCK_VIOLATION) — disk's mounted volumes are
    ///     active; need an FSCTL_LOCK_VOLUME / FSCTL_DISMOUNT first.
    pub fn write_at(&mut self, relative_offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let abs = self.base_offset + relative_offset;
        let ss = self.sector_size;
        // Fast path: already sector-aligned in both offset and length. This
        // is every streamed data chunk (chunk offsets and lengths are
        // multiples of CHUNK_SIZE, itself a multiple of any sector size).
        if abs.is_multiple_of(ss) && (data.len() as u64).is_multiple_of(ss) {
            return self.write_raw(abs, data);
        }
        // Slow path: a misaligned write (e.g. a 512-byte boot-sector patch
        // landing on a 4Kn disk). Grow the write to the enclosing aligned
        // span, read the existing sectors, splice our bytes in, and write
        // the whole span back. Without this, WriteFile returns Win32 error
        // 87 (ERROR_INVALID_PARAMETER) on raw-disk handles.
        let aligned_start = abs - (abs % ss);
        let aligned_end = (abs + data.len() as u64).div_ceil(ss) * ss;
        let span = (aligned_end - aligned_start) as usize;
        let mut buf = vec![0u8; span];
        self.read_raw(aligned_start, &mut buf)?;
        let inner = (abs - aligned_start) as usize;
        buf[inner..inner + data.len()].copy_from_slice(data);
        self.write_raw(aligned_start, &buf)
    }

    /// Absolute-offset raw write. `abs` and `data.len()` must be
    /// sector-aligned; callers reach this only via `write_at`, which
    /// guarantees alignment (directly on the fast path, or by bouncing
    /// through an aligned span on the slow path).
    fn write_raw(&mut self, abs: u64, data: &[u8]) -> Result<()> {
        unsafe {
            let mut dist = 0i64;
            if windows_sys::Win32::Storage::FileSystem::SetFilePointerEx(
                self.handle,
                abs as i64,
                &mut dist,
                0,
            ) == 0
            {
                let err = GetLastError();
                return Err(PhoenixError::Disk(format!(
                    "SetFilePointerEx to disk offset {abs} failed (Win32 error {err})"
                )));
            }
            let mut written = 0u32;
            if windows_sys::Win32::Storage::FileSystem::WriteFile(
                self.handle,
                data.as_ptr() as *const _,
                data.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            ) == 0
            {
                let err = GetLastError();
                return Err(PhoenixError::Disk(format!(
                    "WriteFile of {} bytes at disk offset {abs} failed (Win32 error {err})",
                    data.len()
                )));
            }
            if written as usize != data.len() {
                return Err(PhoenixError::Disk(format!(
                    "WriteFile short write at disk offset {abs}: wrote {written} of {} bytes",
                    data.len()
                )));
            }
        }
        Ok(())
    }
}

impl Drop for PartitionWriter {
    fn drop(&mut self) {
        // Best-effort flush before closing so a caller that forgot an
        // explicit flush() (or bailed on an error path) still commits any
        // buffered writes. Raw-disk handles are effectively unbuffered, so
        // this is belt-and-suspenders; ignore the result since Drop can't
        // surface an error and CloseHandle itself flushes on close.
        unsafe {
            windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(self.handle);
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

pub fn raw_extent_for_partition(size_bytes: u64, sector_size: u32) -> Vec<Extent> {
    let sectors = size_bytes.div_ceil(sector_size as u64);
    vec![Extent {
        start_sector: 0,
        sector_count: sectors,
    }]
}

#[cfg(test)]
mod tests {
    use super::clamp_write;

    #[test]
    fn clamp_write_bounds() {
        // Fully within bounds: write everything.
        assert_eq!(clamp_write(0, 100, 1000), Some(100));
        // Straddles the boundary: write only up to max_bytes.
        assert_eq!(clamp_write(900, 200, 1000), Some(100));
        // Starts exactly at the boundary: write nothing.
        assert_eq!(clamp_write(1000, 50, 1000), None);
        // Starts past the boundary: write nothing.
        assert_eq!(clamp_write(1200, 50, 1000), None);
        // Ends exactly at the boundary: write everything.
        assert_eq!(clamp_write(950, 50, 1000), Some(50));
    }
}
