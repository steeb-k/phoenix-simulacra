//! Random-access read engine for a `.phnx` backup, presented as a flat disk.
//!
//! A backup stores partitions as compressed used-block streams; this turns them
//! back into a byte-addressable disk image on demand. Given an absolute disk
//! offset it locates the owning partition, the extent within it, and the 4 MiB
//! chunk within that extent, decompresses the chunk, **verifies its BLAKE3
//! against the manifest**, and returns the requested slice. Regions that were
//! never captured (free space, inter-partition gaps, unallocated tail) read as
//! zeros. A corrupt chunk surfaces as an error rather than silent garbage.

use std::collections::HashMap;

use blake3;
use phoenix_core::container::{ChunkIndex, PhnxReader, EXTENT_LBA_BYTES};
use phoenix_core::error::{PhoenixError, Result};

/// Placement of one partition on the synthesized disk.
#[derive(Debug, Clone)]
pub struct PartitionSpan {
    pub partition_index: u32,
    /// Absolute byte offset of the partition on the synthesized disk.
    pub disk_offset: u64,
    /// Partition size in bytes.
    pub size: u64,
}

/// Per-extent placement inside a partition (all partition-relative bytes).
#[derive(Debug, Clone)]
struct ExtentSpan {
    start: u64,
    end: u64,
    /// Chunks in stream order with their cumulative byte offset within the
    /// extent. Chunks are contiguous from extent byte 0 but can be ANY
    /// length — capture emits a short chunk whenever a source read returns
    /// short (observed on locked USB volumes) — so chunk lookup must go by
    /// these offsets, never by a fixed `CHUNK_SIZE` stride.
    chunks: Vec<PlacedChunk>,
}

/// One chunk placed at its byte offset within its extent.
#[derive(Debug, Clone)]
struct PlacedChunk {
    /// Byte offset within the extent where this chunk's data begins.
    start: u64,
    chunk: ChunkIndex,
    /// Expected BLAKE3 (hex) from the manifest record paired with this chunk.
    /// Pairing is POSITIONAL (`paired_chunks`, same as verify/restore): the
    /// stream table numbers `chunk_index` per extent while manifest records
    /// number it globally across the partition, so a key join on
    /// `(extent_index, chunk_index)` pairs the wrong records — that bug
    /// surfaced as hash-mismatch read errors on every multi-extent partition
    /// and Windows mounting the volume as RAW.
    blake3: String,
}

struct PartitionIndex {
    span: PartitionSpan,
    extents: Vec<ExtentSpan>,
}

pub struct ChunkStore {
    reader: PhnxReader,
    partitions: Vec<PartitionIndex>,
    total_size: u64,
    /// Small decompressed-chunk cache keyed by file offset.
    cache: HashMap<u64, Vec<u8>>,
    cache_order: Vec<u64>,
    cache_cap: usize,
}

impl ChunkStore {
    /// Build a store over `reader` with partitions placed per `layout`. The
    /// synthesized disk is `disk_size` bytes (must cover the last partition).
    pub fn new(mut reader: PhnxReader, layout: Vec<PartitionSpan>, disk_size: u64) -> Result<Self> {
        let mut partitions = Vec::with_capacity(layout.len());
        for span in layout {
            let entry = reader
                .index
                .iter()
                .find(|e| e.index == span.partition_index)
                .cloned()
                .ok_or_else(|| {
                    PhoenixError::InvalidFormat(format!(
                        "layout references partition {} not in backup",
                        span.partition_index
                    ))
                })?;
            let stream = reader.read_stream_header(&entry)?;
            let records = reader
                .manifest
                .partitions
                .iter()
                .find(|p| p.index == span.partition_index)
                .map(|p| p.chunks.clone())
                .ok_or_else(|| PhoenixError::Manifest("partition manifest missing".into()))?;

            // Pair each stream chunk with its manifest record positionally
            // (the only pairing the format guarantees — see `PlacedChunk`),
            // then group by extent in stream order. Chunks fill their extent
            // contiguously from byte 0, so cumulative lengths place them.
            let mut by_extent: HashMap<u32, Vec<PlacedChunk>> = HashMap::new();
            for (chunk, rec) in phoenix_core::container::paired_chunks(
                &stream.chunks,
                &records,
                span.partition_index,
            )? {
                let placed = by_extent.entry(chunk.extent_index).or_default();
                let at = placed
                    .last()
                    .map(|p| p.start + p.chunk.uncompressed_len as u64)
                    .unwrap_or(0);
                placed.push(PlacedChunk {
                    start: at,
                    chunk: chunk.clone(),
                    blake3: rec.blake3.clone(),
                });
            }

            let mut extents = Vec::with_capacity(stream.extents.len());
            for (ei, ext) in stream.extents.iter().enumerate() {
                let start = ext.start_sector * EXTENT_LBA_BYTES as u64;
                let end = start + ext.sector_count * EXTENT_LBA_BYTES as u64;
                let chunks = by_extent.remove(&(ei as u32)).unwrap_or_default();
                extents.push(ExtentSpan { start, end, chunks });
            }
            extents.sort_by_key(|e| e.start);

            partitions.push(PartitionIndex { span, extents });
        }
        partitions.sort_by_key(|p| p.span.disk_offset);

        Ok(Self {
            reader,
            partitions,
            total_size: disk_size,
            cache: HashMap::new(),
            cache_order: Vec::new(),
            cache_cap: 64, // 64 * 4 MiB = 256 MiB ceiling
        })
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// Fill `buf` with the bytes at absolute disk `offset`, zero-filling any
    /// region that isn't backed by captured data.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        buf.fill(0);
        let mut done = 0usize;
        while done < buf.len() {
            let abs = offset + done as u64;
            if abs >= self.total_size {
                break; // past the end of the disk: leave zeros
            }
            // Which partition (if any) owns this byte?
            let part_idx = self
                .partitions
                .iter()
                .position(|p| abs >= p.span.disk_offset && abs < p.span.disk_offset + p.span.size);
            let Some(pi) = part_idx else {
                // Inter-partition gap: advance to the next partition start (or
                // end of disk) leaving zeros.
                let next = self
                    .partitions
                    .iter()
                    .map(|p| p.span.disk_offset)
                    .filter(|&o| o > abs)
                    .min()
                    .unwrap_or(self.total_size);
                let skip = ((next - abs) as usize).min(buf.len() - done);
                done += skip;
                continue;
            };
            done += self.read_within_partition(pi, abs, &mut buf[done..])?;
        }
        Ok(())
    }

    /// Read as much as possible for one contiguous run inside partition `pi`
    /// starting at absolute `abs`. Returns bytes filled (>=1 unless at EOF).
    fn read_within_partition(&mut self, pi: usize, abs: u64, out: &mut [u8]) -> Result<usize> {
        let (disk_offset, part_size) = {
            let p = &self.partitions[pi];
            (p.span.disk_offset, p.span.size)
        };
        let rel = abs - disk_offset;
        let part_remaining = (part_size - rel) as usize;
        let want = out.len().min(part_remaining);

        // Find the extent covering `rel`, if any.
        let extent = self.partitions[pi]
            .extents
            .iter()
            .find(|e| rel >= e.start && rel < e.end)
            .cloned();
        let Some(extent) = extent else {
            // Free space within the partition: zero-fill up to the next extent
            // (or the partition end). `out` is already zeroed by the caller.
            let gap_len = self.partitions[pi]
                .extents
                .iter()
                .map(|e| e.start)
                .filter(|&s| s > rel)
                .min()
                .map(|s| s - rel)
                .unwrap_or(part_remaining as u64);
            return Ok(want.min(gap_len as usize).max(1).min(want));
        };

        // Locate the chunk covering `rel` by its cumulative offset within the
        // extent (chunks can be any length, see `ExtentSpan::chunks`).
        let in_extent = rel - extent.start;
        let at = extent.chunks.partition_point(|c| c.start <= in_extent);
        let placed = at
            .checked_sub(1)
            .and_then(|i| extent.chunks.get(i))
            .filter(|c| in_extent < c.start + c.chunk.uncompressed_len as u64)
            .cloned()
            .ok_or_else(|| PhoenixError::TableCorrupt {
                what: format!(
                    "partition {}: no chunk covers extent byte {} (absolute byte {})",
                    self.partitions[pi].span.partition_index, in_extent, rel
                ),
            })?;
        let off_in_chunk = (in_extent - placed.start) as usize;

        let data = self.load_chunk(pi, &placed)?;
        let avail = data.len().saturating_sub(off_in_chunk);
        let n = want.min(avail);
        if n == 0 {
            return Err(PhoenixError::TableCorrupt {
                what: format!(
                    "partition {}: chunk at extent byte {} shorter than expected",
                    self.partitions[pi].span.partition_index, in_extent
                ),
            });
        }
        out[..n].copy_from_slice(&data[off_in_chunk..off_in_chunk + n]);
        Ok(n)
    }

    /// Decompress + BLAKE3-verify a chunk (using the cache when possible).
    fn load_chunk(&mut self, pi: usize, placed: &PlacedChunk) -> Result<Vec<u8>> {
        let chunk = &placed.chunk;
        if let Some(cached) = self.cache.get(&chunk.file_offset) {
            return Ok(cached.clone());
        }
        let data = self.reader.read_chunk(chunk)?;
        // Verify against the manifest record paired with this chunk.
        let got = blake3::hash(&data).to_hex().to_string();
        if got != placed.blake3 {
            return Err(PhoenixError::HashMismatch {
                partition_index: self.partitions[pi].span.partition_index,
                chunk_index: chunk.chunk_index,
            });
        }
        // Insert into the LRU cache.
        if self.cache_order.len() >= self.cache_cap {
            if let Some(old) = self.cache_order.first().copied() {
                self.cache.remove(&old);
                self.cache_order.remove(0);
            }
        }
        self.cache.insert(chunk.file_offset, data.clone());
        self.cache_order.push(chunk.file_offset);
        Ok(data)
    }
}

/// Lay partitions out sequentially on a synthesized disk, each aligned to
/// `align` bytes, and return the spans plus the total disk size. Mirrors the
/// on-disk placement a restore would produce so the synthesized GPT matches.
pub fn plan_layout(reader: &PhnxReader, align: u64) -> (Vec<PartitionSpan>, u64) {
    let mut spans = Vec::new();
    let mut offset = align; // leave room for the GPT at the front
    for e in &reader.index {
        spans.push(PartitionSpan {
            partition_index: e.index,
            disk_offset: offset,
            size: e.original_size,
        });
        offset += align_up(e.original_size, align);
    }
    // Reserve a little tail for the backup GPT.
    let disk_size = offset + align;
    (spans, disk_size)
}

fn align_up(v: u64, a: u64) -> u64 {
    if a == 0 {
        v
    } else {
        v.div_ceil(a) * a
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::container::{Extent, Header, PhnxWriter, CHUNK_SIZE, FORMAT_VERSION};
    use phoenix_core::disk::{CaptureMode, FilesystemKind};
    use phoenix_core::manifest::{BackupManifest, DiskManifest, PartitionManifest};
    use uuid::Uuid;

    /// One partition, 128 KiB, with data only in the first 64 KiB extent (the
    /// second 64 KiB is free space that must read back as zeros).
    fn build_backup() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("cs_{}.phnx", Uuid::new_v4()));
        let backup_id = Uuid::new_v4();
        let header = Header {
            version: FORMAT_VERSION,
            flags: 1,
            timestamp: 1,
            backup_id,
            disk_signature: 1,
            partition_count: 1,
        };
        let ext_bytes = 64 * 1024u64;
        // Extent at partition byte 0..64K; partition size is 128K (rest free).
        let extents = vec![Extent {
            start_sector: 0,
            sector_count: ext_bytes / EXTENT_LBA_BYTES as u64,
        }];
        let mut w = PhnxWriter::create(&path, header).unwrap();
        let mut s = w
            .begin_partition_stream(
                0,
                [0; 16],
                "P".into(),
                128 * 1024,
                FilesystemKind::Ntfs,
                CaptureMode::UsedBlocks,
                EXTENT_LBA_BYTES,
                0,
                &extents,
                4096,
            )
            .unwrap();
        s.write_chunk(&vec![0x7Eu8; ext_bytes as usize]).unwrap();
        let (chunks, _) = s.finish().unwrap();
        let manifest = BackupManifest {
            format_version: 1,
            backup_id,
            parent_backup_id: None,
            hostname: "T".into(),
            disk: DiskManifest {
                style: "gpt".into(),
                disk_guid: None,
                sector_size: 512,
            },
            partitions: vec![PartitionManifest {
                index: 0,
                name: "P".into(),
                type_guid: None,
                fs: "ntfs".into(),
                capture_mode: "used-blocks".into(),
                original_size: 128 * 1024,
                used_bytes: ext_bytes,
                bitlocker: None,
                unique_guid: None,
                gpt_attributes: None,
                chunks,
                bitmap_hash: None,
            }],
        };
        w.finalize(&manifest).unwrap();
        path
    }

    #[test]
    fn reads_data_and_zero_fills_gaps() {
        let path = build_backup();
        let reader = PhnxReader::open(&path).unwrap();
        let (layout, disk_size) = plan_layout(&reader, 1024 * 1024);
        let poff = layout[0].disk_offset;
        let mut store = ChunkStore::new(reader, layout, disk_size).unwrap();

        // Backed region (first 64 KiB) reads the captured 0x7E bytes.
        let mut buf = vec![0u8; 64 * 1024];
        store.read_at(poff, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x7E), "backed region not read");

        // Free region (next 64 KiB) reads as zeros.
        let mut buf2 = vec![0xFFu8; 64 * 1024];
        store.read_at(poff + 64 * 1024, &mut buf2).unwrap();
        assert!(buf2.iter().all(|&b| b == 0), "free region not zero-filled");

        // Region before the partition (the GPT gap) reads as zeros.
        let mut buf3 = vec![0xFFu8; 512];
        store.read_at(0, &mut buf3).unwrap();
        assert!(buf3.iter().all(|&b| b == 0));

        std::fs::remove_file(&path).ok();
    }

    /// Regression test for the RAW-volume mount bug (the real 32GB-USB
    /// repro): the stream table numbers `chunk_index` per extent while the
    /// manifest numbers it globally across the partition, so pairing hashes
    /// by `(extent_index, chunk_index)` key verified extent 1+ chunks
    /// against the wrong records — every read of a multi-extent partition
    /// failed with a hash mismatch and Windows surfaced the NTFS volume as
    /// RAW. Pairing must be positional (`paired_chunks`), like verify and
    /// restore. This writes through the real `set_extent`/`write_chunk`
    /// path, which produces exactly that numbering divergence.
    #[test]
    fn multi_extent_backup_reads_correctly() {
        let path = std::env::temp_dir().join(format!("cs_{}.phnx", Uuid::new_v4()));
        let backup_id = Uuid::new_v4();
        let header = Header {
            version: FORMAT_VERSION,
            flags: 1,
            timestamp: 1,
            backup_id,
            disk_signature: 1,
            partition_count: 1,
        };
        // Extent 0: one 64 KiB chunk. Extent 1 (at 16 MiB): a full 4 MiB
        // chunk + a 64 KiB tail chunk. Stream numbering: (0,0) (1,0) (1,1);
        // manifest numbering: (0,0) (1,1) (1,2) — the keys overlap on (1,1)
        // with DIFFERENT data, which is what made the key join blow up.
        let e0_bytes = 64 * 1024u64;
        let e1_bytes = (CHUNK_SIZE + 64 * 1024) as u64;
        let part_bytes = 32 * 1024 * 1024u64;
        let extents = vec![
            Extent {
                start_sector: 0,
                sector_count: e0_bytes / EXTENT_LBA_BYTES as u64,
            },
            Extent {
                start_sector: (16 * 1024 * 1024) / EXTENT_LBA_BYTES as u64,
                sector_count: e1_bytes / EXTENT_LBA_BYTES as u64,
            },
        ];
        let mut w = PhnxWriter::create(&path, header).unwrap();
        let mut s = w
            .begin_partition_stream(
                0,
                [0; 16],
                "P".into(),
                part_bytes,
                FilesystemKind::Ntfs,
                CaptureMode::UsedBlocks,
                EXTENT_LBA_BYTES,
                0,
                &extents,
                4096,
            )
            .unwrap();
        s.set_extent(0);
        s.write_chunk(&vec![0x33u8; e0_bytes as usize]).unwrap();
        s.set_extent(1);
        s.write_chunk(&vec![0x44u8; CHUNK_SIZE]).unwrap();
        s.write_chunk(&vec![0x55u8; 64 * 1024]).unwrap();
        let (chunks, _) = s.finish().unwrap();
        let manifest = BackupManifest {
            format_version: 1,
            backup_id,
            parent_backup_id: None,
            hostname: "T".into(),
            disk: DiskManifest {
                style: "mbr".into(),
                disk_guid: None,
                sector_size: 512,
            },
            partitions: vec![PartitionManifest {
                index: 0,
                name: "P".into(),
                type_guid: None,
                fs: "ntfs".into(),
                capture_mode: "used-blocks".into(),
                original_size: part_bytes,
                used_bytes: e0_bytes + e1_bytes,
                bitlocker: None,
                unique_guid: None,
                gpt_attributes: None,
                chunks,
                bitmap_hash: None,
            }],
        };
        w.finalize(&manifest).unwrap();

        let reader = PhnxReader::open(&path).unwrap();
        let (layout, disk_size) = plan_layout(&reader, 1024 * 1024);
        let poff = layout[0].disk_offset;
        let mut store = ChunkStore::new(reader, layout, disk_size).unwrap();

        // Extent 0 data.
        let mut buf = vec![0u8; e0_bytes as usize];
        store.read_at(poff, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0x33), "extent 0 data wrong");

        // Extent 1: read across the whole extent (both chunks) — this is the
        // read that used to fail with a hash mismatch.
        let e1_off = poff + 16 * 1024 * 1024;
        let mut buf1 = vec![0u8; e1_bytes as usize];
        store.read_at(e1_off, &mut buf1).unwrap();
        assert!(
            buf1[..CHUNK_SIZE].iter().all(|&b| b == 0x44),
            "extent 1 chunk 0 data wrong"
        );
        assert!(
            buf1[CHUNK_SIZE..].iter().all(|&b| b == 0x55),
            "extent 1 chunk 1 data wrong"
        );

        std::fs::remove_file(&path).ok();
    }

    /// Regression test for the RAW-volume mount bug: capture emits a chunk
    /// SHORTER than `CHUNK_SIZE` mid-extent whenever a source read returns
    /// short (observed on locked USB volumes — `capture_ntfs` writes
    /// `&buf[..n]`). Chunk lookup must therefore go by cumulative chunk
    /// lengths; the old fixed-stride `offset / CHUNK_SIZE` math shifted every
    /// chunk after the short one, serving wrong bytes / hash mismatches.
    #[test]
    fn short_mid_extent_chunk_reads_correctly() {
        let path = std::env::temp_dir().join(format!("cs_{}.phnx", Uuid::new_v4()));
        let backup_id = Uuid::new_v4();
        let header = Header {
            version: FORMAT_VERSION,
            flags: 1,
            timestamp: 1,
            backup_id,
            disk_signature: 1,
            partition_count: 1,
        };
        // One 128 KiB extent captured as a SHORT 48 KiB chunk + an 80 KiB rest.
        let part_bytes = 128 * 1024u64;
        let short_len = 48 * 1024usize;
        let extents = vec![Extent {
            start_sector: 0,
            sector_count: part_bytes / EXTENT_LBA_BYTES as u64,
        }];
        let mut w = PhnxWriter::create(&path, header).unwrap();
        let mut s = w
            .begin_partition_stream(
                0,
                [0; 16],
                "P".into(),
                part_bytes,
                FilesystemKind::Ntfs,
                CaptureMode::UsedBlocks,
                EXTENT_LBA_BYTES,
                0,
                &extents,
                4096,
            )
            .unwrap();
        s.write_chunk(&vec![0x11u8; short_len]).unwrap();
        s.write_chunk(&vec![0x22u8; part_bytes as usize - short_len])
            .unwrap();
        let (chunks, _) = s.finish().unwrap();
        let manifest = BackupManifest {
            format_version: 1,
            backup_id,
            parent_backup_id: None,
            hostname: "T".into(),
            disk: DiskManifest {
                style: "mbr".into(),
                disk_guid: None,
                sector_size: 512,
            },
            partitions: vec![PartitionManifest {
                index: 0,
                name: "P".into(),
                type_guid: None,
                fs: "ntfs".into(),
                capture_mode: "used-blocks".into(),
                original_size: part_bytes,
                used_bytes: part_bytes,
                bitlocker: None,
                unique_guid: None,
                gpt_attributes: None,
                chunks,
                bitmap_hash: None,
            }],
        };
        w.finalize(&manifest).unwrap();

        let reader = PhnxReader::open(&path).unwrap();
        let (layout, disk_size) = plan_layout(&reader, 1024 * 1024);
        let poff = layout[0].disk_offset;
        let mut store = ChunkStore::new(reader, layout, disk_size).unwrap();

        // Read the whole partition in one go and check both chunk regions,
        // then a small read straddling the short-chunk boundary.
        let mut buf = vec![0u8; part_bytes as usize];
        store.read_at(poff, &mut buf).unwrap();
        assert!(
            buf[..short_len].iter().all(|&b| b == 0x11),
            "short chunk data wrong"
        );
        assert!(
            buf[short_len..].iter().all(|&b| b == 0x22),
            "data after the short chunk must come from the NEXT chunk"
        );

        let mut straddle = vec![0u8; 8192];
        store.read_at(poff + short_len as u64 - 4096, &mut straddle).unwrap();
        assert!(straddle[..4096].iter().all(|&b| b == 0x11));
        assert!(straddle[4096..].iter().all(|&b| b == 0x22));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn corrupt_chunk_is_rejected() {
        let path = build_backup();
        // Corrupt a byte in the chunk payload region.
        let off = {
            let mut r = PhnxReader::open(&path).unwrap();
            let e = r.index[0].clone();
            r.read_stream_header(&e).unwrap().chunks[0].file_offset
        };
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(off + 3)).unwrap();
            f.write_all(&[0xAA]).unwrap();
        }
        let reader = PhnxReader::open(&path).unwrap();
        let (layout, disk_size) = plan_layout(&reader, 1024 * 1024);
        let poff = layout[0].disk_offset;
        let mut store = ChunkStore::new(reader, layout, disk_size).unwrap();
        let mut buf = vec![0u8; 4096];
        let err = store.read_at(poff, &mut buf);
        assert!(err.is_err(), "corrupt chunk must surface as an error");
        std::fs::remove_file(&path).ok();
    }
}
