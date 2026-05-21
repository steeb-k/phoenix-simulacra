use std::os::windows::ffi::OsStrExt;

use phoenix_core::container::{Extent, CHUNK_SIZE};
use phoenix_core::error::Result;
use phoenix_core::manifest::ChunkRecord;
use phoenix_core::ProgressHandle;

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

pub fn restore_raw(
    reader: &mut phoenix_core::container::PhnxReader,
    entry: &phoenix_core::container::PartitionIndexEntry,
    writer: &mut PartitionWriter,
    verify: bool,
    progress: Option<&ProgressHandle>,
    mut chunks_done: u64,
) -> Result<u64> {
    let stream = reader.read_stream_header(entry)?;
    let chunk_records: Vec<phoenix_core::manifest::ChunkRecord> = reader
        .manifest
        .partitions
        .iter()
        .find(|p| p.index == entry.index)
        .map(|p| p.chunks.clone())
        .ok_or_else(|| phoenix_core::error::PhoenixError::Manifest("missing partition".into()))?;

    let sector_size = entry.sector_size as u64;
    for (chunk, record) in stream.chunks.iter().zip(chunk_records.iter()) {
        let data = reader.read_chunk(chunk)?;
        if verify {
            let computed = phoenix_core::hash::hash_hex(&data);
            if computed != record.blake3 {
                return Err(phoenix_core::error::PhoenixError::HashMismatch {
                    partition_index: entry.index,
                    chunk_index: record.chunk_index,
                });
            }
        }
        let extent = &stream.extents[chunk.extent_index as usize];
        let offset = extent.start_sector * sector_size;
        writer.write_at(offset, &data)?;
        chunks_done += 1;
        if let Some(p) = progress {
            let total = p.snapshot().total;
            p.set(chunks_done, format!("Chunk {chunks_done} / {total}"));
        }
    }
    Ok(chunks_done)
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
                return Err(phoenix_core::error::PhoenixError::Disk("seek failed".into()));
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
                return Err(phoenix_core::error::PhoenixError::Disk("write failed".into()));
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
