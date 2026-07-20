//! Partition-relative random access into one partition of a `.phnx`.
//!
//! [`PhnxReader`] reads a *chunk* at a time; restoring streams those chunks
//! in order and never needs to seek. The shrink pre-flight does: it has to
//! read the source volume's `$MFT` — a few hundred scattered clusters of
//! boot sector and MFT records — out of a backup, before deciding whether
//! the shrink is possible at all. This turns a partition's chunk streams
//! back into a byte-addressable view for exactly that.
//!
//! Regions the capture never stored (free space between extents, the tail
//! past the last extent) read as zeros, matching what a restore would land
//! on disk. A chunk whose BLAKE3 disagrees with the manifest is an error,
//! never silent garbage.
//!
//! `phoenix-mount`'s [`ChunkStore`] does the same job for a whole
//! synthesized *disk*, with a large cache and readahead tuned for a booting
//! VM guest hammering it with scattered 4K reads. This is deliberately not
//! that: one partition, no disk layout, a small cache, and — importantly —
//! it *borrows* the reader instead of consuming it, because the restore
//! path still needs that reader afterwards to stream the data.
//!
//! [`ChunkStore`]: https://docs.rs/phoenix-mount

use std::collections::VecDeque;

use crate::container::{
    paired_chunks, ChunkIndex, PartitionIndexEntry, PhnxReader, EXTENT_LBA_BYTES,
};
use crate::error::{PhoenixError, Result};

/// How many decompressed chunks to keep. The pre-flight walks the MFT
/// front to back, so it revisits a chunk only around extent boundaries and
/// when re-reading record 0 — a handful of entries is plenty, and each one
/// costs up to a full chunk of memory.
const CACHE_CAP: usize = 16;

/// One chunk placed at its byte offset within its extent.
#[derive(Debug, Clone, Copy)]
struct PlacedChunk {
    /// Byte offset within the extent where this chunk's data begins.
    start: u64,
    chunk: ChunkIndex,
    /// Expected BLAKE3 from the manifest record paired with this chunk,
    /// decoded so verification is a 32-byte compare.
    blake3: [u8; 32],
}

/// One captured extent, in partition-relative bytes.
#[derive(Debug, Clone)]
struct ExtentSpan {
    start: u64,
    end: u64,
    /// Chunks in stream order with their cumulative offset within the
    /// extent. Chunks fill the extent contiguously from byte 0 but may be
    /// ANY length — capture emits a short chunk whenever a source read
    /// comes back short — so lookup goes by these offsets, never by a
    /// fixed `CHUNK_SIZE` stride.
    chunks: Vec<PlacedChunk>,
}

fn parse_blake3_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        *slot = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// Byte-addressable view of one partition inside a `.phnx`.
pub struct PartitionByteReader<'a> {
    reader: &'a mut PhnxReader,
    partition_index: u32,
    extents: Vec<ExtentSpan>,
    partition_size: u64,
    cache: Vec<(u64, Vec<u8>)>,
    cache_order: VecDeque<u64>,
}

impl<'a> PartitionByteReader<'a> {
    /// Index `entry`'s stream so its bytes can be read by offset.
    ///
    /// This reads the stream header and the manifest only; no chunk data is
    /// decompressed until the first [`read_at`](Self::read_at).
    pub fn new(reader: &'a mut PhnxReader, entry: &PartitionIndexEntry) -> Result<Self> {
        let partition_index = entry.index;
        let stream = reader.read_stream_header(entry)?;
        let records = reader
            .manifest
            .partitions
            .iter()
            .find(|p| p.index == partition_index)
            .map(|p| p.chunks.clone())
            .ok_or_else(|| {
                PhoenixError::Manifest(format!("partition {partition_index} has no manifest entry"))
            })?;

        // Pair each stream chunk with its manifest record POSITIONALLY. The
        // stream table numbers `chunk_index` per extent while the manifest
        // numbers it globally across the partition, so joining on
        // `(extent_index, chunk_index)` pairs the wrong records — that bug
        // surfaced as hash mismatches on every multi-extent partition.
        let mut by_extent: Vec<Vec<PlacedChunk>> = vec![Vec::new(); stream.extents.len()];
        for (chunk, rec) in paired_chunks(&stream.chunks, &records, partition_index)? {
            let slot = by_extent
                .get_mut(chunk.extent_index as usize)
                .ok_or_else(|| PhoenixError::TableCorrupt {
                    what: format!(
                        "partition {partition_index}: chunk names extent {} but the stream has \
                         only {} extents",
                        chunk.extent_index,
                        stream.extents.len()
                    ),
                })?;
            let start = slot
                .last()
                .map(|p: &PlacedChunk| p.start + p.chunk.uncompressed_len as u64)
                .unwrap_or(0);
            let blake3 =
                parse_blake3_hex(&rec.blake3).ok_or_else(|| PhoenixError::TableCorrupt {
                    what: format!(
                        "partition {partition_index}: chunk {} has a malformed BLAKE3 in the \
                         manifest ({:?})",
                        chunk.chunk_index, rec.blake3
                    ),
                })?;
            slot.push(PlacedChunk {
                start,
                chunk: *chunk,
                blake3,
            });
        }

        let mut extents = Vec::with_capacity(stream.extents.len());
        for (ei, ext) in stream.extents.iter().enumerate() {
            let start = ext.start_sector * EXTENT_LBA_BYTES as u64;
            let end = start + ext.sector_count * EXTENT_LBA_BYTES as u64;
            extents.push(ExtentSpan {
                start,
                end,
                chunks: std::mem::take(&mut by_extent[ei]),
            });
        }
        extents.sort_by_key(|e| e.start);

        Ok(Self {
            reader,
            partition_index,
            partition_size: entry.original_size,
            extents,
            cache: Vec::new(),
            cache_order: VecDeque::new(),
        })
    }

    /// The partition's size as captured, in bytes.
    pub fn len(&self) -> u64 {
        self.partition_size
    }

    pub fn is_empty(&self) -> bool {
        self.partition_size == 0
    }

    /// Fill `buf` from partition-relative `offset`, zero-filling anything
    /// that was never captured.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        buf.fill(0);
        let mut done = 0usize;
        while done < buf.len() {
            let rel = offset + done as u64;
            if rel >= self.partition_size {
                break; // past the captured partition: leave zeros
            }
            done += self.read_one(rel, &mut buf[done..])?;
        }
        Ok(())
    }

    /// Serve as much of `out` as one extent/chunk can. Returns bytes filled,
    /// always at least 1 until the partition end.
    fn read_one(&mut self, rel: u64, out: &mut [u8]) -> Result<usize> {
        let want = out.len().min((self.partition_size - rel) as usize);

        // Locate under an immutable borrow, copying out the (Copy, heap-free)
        // PlacedChunk so the borrow ends before the cache needs `&mut self`.
        let found = self
            .extents
            .iter()
            .position(|e| rel >= e.start && rel < e.end);
        let Some(ei) = found else {
            // Uncaptured free space: skip to the next extent, leaving zeros.
            let next = self
                .extents
                .iter()
                .map(|e| e.start)
                .find(|&s| s > rel)
                .unwrap_or(self.partition_size);
            return Ok(want.min((next - rel) as usize).max(1).min(want));
        };
        let extent = &self.extents[ei];
        let in_extent = rel - extent.start;
        let at = extent.chunks.partition_point(|c| c.start <= in_extent);
        let placed = at
            .checked_sub(1)
            .and_then(|i| extent.chunks.get(i))
            .filter(|c| in_extent < c.start + c.chunk.uncompressed_len as u64)
            .copied()
            .ok_or_else(|| PhoenixError::TableCorrupt {
                what: format!(
                    "partition {}: no chunk covers extent byte {in_extent} (partition byte {rel})",
                    self.partition_index
                ),
            })?;

        let off_in_chunk = (in_extent - placed.start) as usize;
        let data = self.load_chunk(&placed)?;
        let avail = data.len().saturating_sub(off_in_chunk);
        let n = want.min(avail);
        if n == 0 {
            return Err(PhoenixError::TableCorrupt {
                what: format!(
                    "partition {}: chunk at extent byte {in_extent} is shorter than its index \
                     claims",
                    self.partition_index
                ),
            });
        }
        out[..n].copy_from_slice(&data[off_in_chunk..off_in_chunk + n]);
        Ok(n)
    }

    /// Decompress and BLAKE3-verify a chunk, serving it from the cache when
    /// possible. Returns the cache slot's index-free contents by reference.
    fn load_chunk(&mut self, placed: &PlacedChunk) -> Result<&[u8]> {
        let key = placed.chunk.file_offset;
        if let Some(pos) = self.cache.iter().position(|(k, _)| *k == key) {
            self.touch(key);
            return Ok(&self.cache[pos].1);
        }
        let data = self.reader.read_chunk(&placed.chunk)?;
        if blake3::hash(&data).as_bytes() != &placed.blake3 {
            return Err(PhoenixError::HashMismatch {
                partition_index: self.partition_index,
                chunk_index: placed.chunk.chunk_index,
            });
        }
        if self.cache_order.len() >= CACHE_CAP {
            if let Some(old) = self.cache_order.pop_front() {
                self.cache.retain(|(k, _)| *k != old);
            }
        }
        self.cache.push((key, data));
        self.cache_order.push_back(key);
        Ok(&self.cache.last().unwrap().1)
    }

    fn touch(&mut self, key: u64) {
        if let Some(pos) = self.cache_order.iter().rposition(|&k| k == key) {
            if pos + 1 != self.cache_order.len() {
                self.cache_order.remove(pos);
                self.cache_order.push_back(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::{Extent, Header, PartitionStreamSpec, PhnxWriter};
    use crate::disk::{CaptureMode, FilesystemKind};
    use crate::manifest::{BackupManifest, DiskManifest, PartitionManifest};
    use uuid::Uuid;

    /// Write a one-partition `.phnx` whose extents hold position-derived
    /// bytes, and return the partition image those extents should read back
    /// as (uncaptured regions zero).
    fn write_backup(
        path: &std::path::Path,
        extents: &[Extent],
        chunk_split: usize,
        partition_size: u64,
    ) -> Vec<u8> {
        let mut image = vec![0u8; partition_size as usize];
        let backup_id = Uuid::new_v4();
        let header = Header {
            version: 1,
            flags: 0,
            timestamp: 1,
            backup_id,
            disk_signature: 1,
            partition_count: 1,
        };
        let mut writer = PhnxWriter::create(path, header).unwrap();
        let mut stream = writer
            .begin_partition_stream(PartitionStreamSpec {
                index: 0,
                type_guid: [0; 16],
                name: "T".into(),
                original_size: partition_size,
                fs_kind: FilesystemKind::Ntfs,
                capture_mode: CaptureMode::UsedBlocks,
                sector_size: EXTENT_LBA_BYTES,
                used_bytes: 0,
                extents,
                bytes_per_cluster: 4096,
            })
            .unwrap();
        for (ei, ext) in extents.iter().enumerate() {
            stream.set_extent(ei as u32);
            let start = (ext.start_sector * EXTENT_LBA_BYTES as u64) as usize;
            let len = (ext.sector_count * EXTENT_LBA_BYTES as u64) as usize;
            let mut written = 0usize;
            while written < len {
                let take = chunk_split.min(len - written);
                let data: Vec<u8> = (0..take)
                    .map(|i| ((start + written + i) % 251) as u8)
                    .collect();
                image[start + written..start + written + take].copy_from_slice(&data);
                stream.write_chunk(&data).unwrap();
                written += take;
            }
        }
        let (chunks, _) = stream.finish().unwrap();
        let manifest = BackupManifest {
            format_version: 1,
            backup_id,
            parent_backup_id: None,
            hostname: "T".into(),
            disk: DiskManifest {
                style: "gpt".into(),
                disk_guid: None,
                disk_signature: None,
                mbr_boot_code: None,
                sector_size: 512,
            },
            partitions: vec![PartitionManifest {
                index: 0,
                name: "T".into(),
                type_guid: None,
                fs: "ntfs".into(),
                capture_mode: "used-blocks".into(),
                original_size: partition_size,
                used_bytes: 0,
                bitlocker: None,
                unique_guid: None,
                gpt_attributes: None,
                mbr_type: None,
                mbr_bootable: None,
                chunks,
                bitmap_hash: None,
            }],
        };
        writer.finalize(&manifest).unwrap();
        image
    }

    fn ext(start_sector: u64, sector_count: u64) -> Extent {
        Extent {
            start_sector,
            sector_count,
        }
    }

    #[test]
    fn reads_back_every_captured_byte_and_zeros_the_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.phnx");
        // Two extents with a gap between them, so the reader has to zero-fill
        // the middle and still place the second extent's chunks correctly.
        let extents = vec![ext(0, 16), ext(64, 16)];
        let image = write_backup(&path, &extents, 4096, 64 * 1024);

        let mut reader = PhnxReader::open(&path).unwrap();
        let entry = reader.index[0].clone();
        let mut pr = PartitionByteReader::new(&mut reader, &entry).unwrap();

        let mut got = vec![0u8; image.len()];
        pr.read_at(0, &mut got).unwrap();
        assert_eq!(got, image);

        // Scattered small reads must agree with the whole-image read.
        for &off in &[0u64, 1, 4095, 4096, 8191, 32 * 1024, 40 * 1024] {
            let mut small = [0u8; 512];
            pr.read_at(off, &mut small).unwrap();
            assert_eq!(
                &small[..],
                &image[off as usize..off as usize + 512],
                "mismatch at offset {off}"
            );
        }
    }

    #[test]
    fn places_chunks_by_cumulative_offset_not_a_fixed_stride() {
        // Chunks shorter than CHUNK_SIZE (capture emits these when a source
        // read comes back short); a fixed-stride lookup would mis-place them.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.phnx");
        let extents = vec![ext(0, 16), ext(32, 16)];
        let image = write_backup(&path, &extents, 3000, 32 * 1024);

        let mut reader = PhnxReader::open(&path).unwrap();
        let entry = reader.index[0].clone();
        let mut pr = PartitionByteReader::new(&mut reader, &entry).unwrap();
        let mut got = vec![0u8; image.len()];
        pr.read_at(0, &mut got).unwrap();
        assert_eq!(got, image);
    }

    #[test]
    fn a_corrupt_chunk_is_an_error_not_silent_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.phnx");
        let extents = vec![ext(0, 16)];
        write_backup(&path, &extents, 4096, 8 * 1024);

        // Corrupt the manifest's expectation so the stored chunk no longer
        // matches it — the same signal a damaged chunk would produce.
        let mut reader = PhnxReader::open(&path).unwrap();
        reader.manifest.partitions[0].chunks[0].blake3 = "0".repeat(64);
        let entry = reader.index[0].clone();
        let mut pr = PartitionByteReader::new(&mut reader, &entry).unwrap();
        let mut buf = [0u8; 512];
        assert!(matches!(
            pr.read_at(0, &mut buf),
            Err(PhoenixError::HashMismatch { .. })
        ));
    }
}
