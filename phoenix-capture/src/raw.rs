use std::os::windows::ffi::OsStrExt;

use phoenix_core::container::{Extent, CHUNK_SIZE};
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::manifest::ChunkRecord;
use phoenix_core::relocation::RelocationMap;
use phoenix_core::ProgressHandle;
use windows_sys::Win32::Foundation::GetLastError;

use crate::reader::PartitionReader;

pub fn capture_raw(
    reader: &mut PartitionReader,
    stream: &mut phoenix_core::container::PartitionStreamWriter<'_>,
) -> Result<()> {
    let sector_size = 512u64;
    let total_sectors = (reader.length() + sector_size - 1) / sector_size;
    stream.set_extent(0);

    let mut pos = 0u64;
    let mut chunk_buf = vec![0u8; CHUNK_SIZE];
    while pos < reader.length() {
        let to_read = CHUNK_SIZE.min((reader.length() - pos) as usize);
        let n = reader.read_at(pos, &mut chunk_buf[..to_read])?;
        if n == 0 {
            break;
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
    verify: bool,
    progress: Option<&ProgressHandle>,
    mut bytes_done: u64,
    relocation: Option<&RelocationMap>,
) -> Result<u64> {
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
    for (chunk, record) in stream.chunks.iter().zip(chunk_records.iter()) {
        if let Some(p) = progress {
            if p.is_cancelled() {
                return Err(PhoenixError::Cancelled);
            }
        }
        let data = reader.read_chunk(chunk)?;
        if verify {
            let computed = phoenix_core::hash::hash_hex(&data);
            if computed != record.blake3 {
                return Err(PhoenixError::HashMismatch {
                    partition_index: entry.index,
                    chunk_index: record.chunk_index,
                });
            }
        }
        let extent = &stream.extents[chunk.extent_index as usize];
        let src_offset =
            extent.start_sector * sector_size + (chunk.chunk_index as u64) * CHUNK_SIZE as u64;

        match relocation {
            None => {
                writer.write_at(src_offset, &data)?;
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
                    let end = seg.source_offset + seg.len;
                    writer.write_at(seg.dst_byte_offset, &data[seg.source_offset..end])?;
                }
            }
        }
        bytes_done += data.len() as u64;
        bytes_for_partition += data.len() as u64;
        if let Some(p) = progress {
            p.set(
                bytes_done,
                format!(
                    "Chunk {} / {}",
                    record.chunk_index + 1,
                    chunk_records.len()
                ),
            );
        }
    }
    Ok(bytes_for_partition)
}

pub struct PartitionWriter {
    handle: windows_sys::Win32::Foundation::HANDLE,
    base_offset: u64,
}

impl PartitionWriter {
    pub fn open_disk(disk_path: &str, partition_offset: u64) -> Result<Self> {
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
                windows_sys::Win32::Storage::FileSystem::                OPEN_EXISTING,
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
        let abs = self.base_offset + relative_offset;
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
    ///   *  5 (ERROR_ACCESS_DENIED)         — another process holds a
    ///        handle on the volume; usually a re-mount races us.
    ///   * 19 (ERROR_WRITE_PROTECT)         — disk is read-only.
    ///   * 87 (ERROR_INVALID_PARAMETER)     — offset/length not aligned
    ///        to the disk's logical sector size.
    ///   * 33 (ERROR_LOCK_VIOLATION)        — disk's mounted volumes are
    ///        active; need an FSCTL_LOCK_VOLUME / FSCTL_DISMOUNT first.
    pub fn write_at(&mut self, relative_offset: u64, data: &[u8]) -> Result<()> {
        let abs = self.base_offset + relative_offset;
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
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

pub fn raw_extent_for_partition(size_bytes: u64, sector_size: u32) -> Vec<Extent> {
    let sectors = (size_bytes + sector_size as u64 - 1) / sector_size as u64;
    vec![Extent {
        start_sector: 0,
        sector_count: sectors,
    }]
}
