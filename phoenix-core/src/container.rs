use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use uuid::Uuid;

use crate::disk::{CaptureMode, FilesystemKind};
use crate::error::{PhoenixError, Result};
use crate::hash::{self, crc32};
use crate::manifest::{BackupManifest, ChunkRecord};
use crate::progress::ProgressHandle;

pub const MAGIC: &[u8; 4] = b"PHNX";
pub const FOOTER_MAGIC: &[u8; 4] = b"PHNX";
/// Version emitted by the current writer. Readers accept 1..=FORMAT_VERSION.
pub const FORMAT_VERSION: u16 = 2;
/// Oldest version this build can read.
pub const MIN_READ_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 64;
pub const INDEX_ENTRY_SIZE: usize = 160;
/// v1 footer size (magic `END\0`).
pub const FOOTER_SIZE: usize = 72;
/// v2 footer size (magic `END2`): adds total_file_length, index_table_hash,
/// and a footer CRC so truncation and metadata corruption are always caught.
pub const FOOTER_SIZE_V2: usize = 112;
pub const CHUNK_SIZE: usize = 4 * 1024 * 1024;
/// Byte size of one extent-addressing unit ("sector") in the `.phnx`
/// format. This is a fixed format invariant, independent of the source
/// disk's physical sector size: all extent `start_sector`/`sector_count`
/// values and every `PartitionIndexEntry::sector_size` are expressed in
/// these 512-byte units. Restore code multiplies extent sectors by this to
/// recover byte offsets.
pub const EXTENT_LBA_BYTES: u32 = 512;
/// Reserved bytes after header for partition index entries (max 128 partitions).
pub const INDEX_TABLE_RESERVE: u64 = 128 * INDEX_ENTRY_SIZE as u64;

const FOOTER_END_MAGIC: &[u8; 4] = b"END\x00";
const FOOTER_END_MAGIC_V2: &[u8; 4] = b"END2";

#[derive(Debug, Clone)]
pub struct Header {
    pub version: u16,
    pub flags: u16,
    pub timestamp: u64,
    pub backup_id: Uuid,
    pub disk_signature: u64,
    pub partition_count: u32,
}

#[derive(Debug, Clone)]
pub struct PartitionIndexEntry {
    pub index: u32,
    pub type_guid: [u8; 16],
    pub name: String,
    pub original_size: u64,
    pub fs_kind: FilesystemKind,
    pub capture_mode: CaptureMode,
    pub stream_offset: u64,
    pub stream_length: u64,
    pub sector_size: u32,
    pub used_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Footer {
    pub manifest_offset: u64,
    pub manifest_length: u64,
    pub manifest_hash: [u8; 32],
    pub index_offset: u64,
    pub index_count: u32,
    /// Format version of the footer that was read (1 or 2). Writers always
    /// emit 2; v1 files still open, with the v2-only fields left as defaults.
    pub version: u16,
    /// Total on-disk length the writer committed (v2 only; 0 for v1). Compared
    /// against the real file length at open to detect truncation/padding.
    pub total_file_length: u64,
    /// BLAKE3 over the partition index-entry region (v2 only; zeroed for v1).
    pub index_table_hash: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct Extent {
    pub start_sector: u64,
    pub sector_count: u64,
}

#[derive(Debug, Clone)]
pub struct ChunkIndex {
    pub file_offset: u64,
    pub compressed_len: u32,
    pub uncompressed_len: u32,
    pub extent_index: u32,
    pub chunk_index: u32,
}

#[derive(Debug, Clone)]
pub struct StreamHeader {
    pub extents: Vec<Extent>,
    pub chunks: Vec<ChunkIndex>,
    pub bytes_per_cluster: u32,
}

/// Summary returned by [`PhnxReader::verify_structure`].
#[derive(Debug, Clone, Copy)]
pub struct StructureReport {
    pub partitions: usize,
    pub total_chunks: u64,
    pub total_bytes: u64,
}

/// Choose up to `count` distinct chunk indices from `0..len` to spot-check,
/// always including the first and last chunk, with the remainder drawn from a
/// deterministic PRNG so the selection is reproducible for a given `seed`.
fn sample_indices(len: usize, count: usize, seed: u64) -> Vec<usize> {
    use std::collections::BTreeSet;
    if len == 0 {
        return Vec::new();
    }
    let mut set = BTreeSet::new();
    set.insert(0);
    set.insert(len - 1);
    let mut state = seed | 1;
    while set.len() < count.min(len) {
        // SplitMix64 step.
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        set.insert((z % len as u64) as usize);
    }
    set.into_iter().collect()
}

/// Zip a partition's on-disk chunk-index table with its manifest chunk
/// records, refusing to proceed if the two disagree in length.
///
/// Callers (verify and restore) previously used `chunks.iter().zip(records)`,
/// which silently truncates to the shorter of the two — so a backup with a
/// missing or extra chunk record would verify/restore as if nothing were
/// wrong. This helper turns that class of corruption into a hard error.
pub fn paired_chunks<'a>(
    chunks: &'a [ChunkIndex],
    records: &'a [crate::manifest::ChunkRecord],
    partition_index: u32,
) -> Result<
    std::iter::Zip<
        std::slice::Iter<'a, ChunkIndex>,
        std::slice::Iter<'a, crate::manifest::ChunkRecord>,
    >,
> {
    if chunks.len() != records.len() {
        return Err(PhoenixError::ChunkCountMismatch {
            partition_index,
            stream_chunks: chunks.len(),
            manifest_chunks: records.len(),
        });
    }
    Ok(chunks.iter().zip(records.iter()))
}

impl Header {
    pub fn write<W: Write>(&self, w: &mut W) -> Result<()> {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(MAGIC);
        buf[4..6].copy_from_slice(&self.version.to_le_bytes());
        buf[6..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8..16].copy_from_slice(&self.timestamp.to_le_bytes());
        buf[16..32].copy_from_slice(self.backup_id.as_bytes());
        buf[32..40].copy_from_slice(&self.disk_signature.to_le_bytes());
        buf[40..44].copy_from_slice(&self.partition_count.to_le_bytes());
        let crc = crc32(&buf[0..44]);
        buf[44..48].copy_from_slice(&crc.to_le_bytes());
        w.write_all(&buf)?;
        Ok(())
    }

    pub fn read<R: Read>(r: &mut R) -> Result<Self> {
        let mut buf = [0u8; HEADER_SIZE];
        r.read_exact(&mut buf)?;
        if &buf[0..4] != MAGIC {
            return Err(PhoenixError::InvalidFormat("bad magic".into()));
        }
        let version = u16::from_le_bytes(buf[4..6].try_into().unwrap());
        if version < MIN_READ_VERSION || version > FORMAT_VERSION {
            return Err(PhoenixError::InvalidFormat(format!(
                "unsupported .phnx format version {version} (this build reads {MIN_READ_VERSION}..={FORMAT_VERSION})"
            )));
        }
        let flags = u16::from_le_bytes(buf[6..8].try_into().unwrap());
        let timestamp = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let backup_id = Uuid::from_bytes(buf[16..32].try_into().unwrap());
        let disk_signature = u64::from_le_bytes(buf[32..40].try_into().unwrap());
        let partition_count = u32::from_le_bytes(buf[40..44].try_into().unwrap());
        let stored_crc = u32::from_le_bytes(buf[44..48].try_into().unwrap());
        let mut check = buf;
        check[44..48].fill(0);
        if crc32(&check[0..44]) != stored_crc {
            return Err(PhoenixError::InvalidFormat("header CRC mismatch".into()));
        }
        Ok(Self {
            version,
            flags,
            timestamp,
            backup_id,
            disk_signature,
            partition_count,
        })
    }
}

impl PartitionIndexEntry {
    pub fn write<W: Write>(&self, w: &mut W) -> Result<()> {
        let mut buf = [0u8; INDEX_ENTRY_SIZE];
        buf[0..4].copy_from_slice(&self.index.to_le_bytes());
        buf[4..20].copy_from_slice(&self.type_guid);
        let name_utf16: Vec<u16> = self.name.encode_utf16().collect();
        let name_bytes: Vec<u8> = name_utf16
            .iter()
            .take(36)
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let copy_len = name_bytes.len().min(72);
        buf[20..20 + copy_len].copy_from_slice(&name_bytes[..copy_len]);
        buf[112..120].copy_from_slice(&self.original_size.to_le_bytes());
        buf[120] = self.fs_kind as u8;
        buf[121] = self.capture_mode as u8;
        buf[124..132].copy_from_slice(&self.stream_offset.to_le_bytes());
        buf[132..140].copy_from_slice(&self.stream_length.to_le_bytes());
        buf[140..144].copy_from_slice(&self.sector_size.to_le_bytes());
        buf[144..152].copy_from_slice(&self.used_bytes.to_le_bytes());
        // v2: CRC32 over the entry's meaningful bytes (0..152), stored in the
        // formerly-reserved tail. v1 readers ignore it; the v2 reader verifies
        // it so a corrupted index entry is caught before its stream_offset is
        // trusted. (0x00000000 in a v1 file is treated as "no CRC".)
        let crc = crc32(&buf[0..152]);
        buf[152..156].copy_from_slice(&crc.to_le_bytes());
        w.write_all(&buf)?;
        Ok(())
    }

    /// CRC32 over the entry's meaningful bytes, matching what `write` stores at
    /// offset 152. Used by the v2 reader to validate each index entry.
    pub fn compute_crc(&self) -> Result<u32> {
        let mut buf = Vec::new();
        self.write(&mut buf)?;
        Ok(u32::from_le_bytes(buf[152..156].try_into().unwrap()))
    }

    /// The CRC32 stored in a raw 160-byte index entry (bytes 152..156).
    fn stored_crc(raw: &[u8]) -> u32 {
        u32::from_le_bytes(raw[152..156].try_into().unwrap())
    }

    pub fn read<R: Read>(r: &mut R) -> Result<Self> {
        let mut buf = [0u8; INDEX_ENTRY_SIZE];
        r.read_exact(&mut buf)?;
        let index = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let mut type_guid = [0u8; 16];
        type_guid.copy_from_slice(&buf[4..20]);
        let name_utf16: Vec<u16> = buf[20..92]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        let name = String::from_utf16_lossy(&name_utf16);
        let original_size = u64::from_le_bytes(buf[112..120].try_into().unwrap());
        let fs_kind = FilesystemKind::from_u8(buf[120]);
        let capture_mode = CaptureMode::from_u8(buf[121]);
        let stream_offset = u64::from_le_bytes(buf[124..132].try_into().unwrap());
        let stream_length = u64::from_le_bytes(buf[132..140].try_into().unwrap());
        let sector_size = u32::from_le_bytes(buf[140..144].try_into().unwrap());
        let used_bytes = u64::from_le_bytes(buf[144..152].try_into().unwrap());
        Ok(Self {
            index,
            type_guid,
            name,
            original_size,
            fs_kind,
            capture_mode,
            stream_offset,
            stream_length,
            sector_size,
            used_bytes,
        })
    }
}

impl Footer {
    /// Serialize the v2 footer (112 bytes, magic `END2`). Byte layout:
    /// ```text
    ///   0.. 8  manifest_offset       48..56  index_offset
    ///   8..16  manifest_length       56..60  index_count
    ///  16..48  manifest_hash (32)    60..64  format_version (u32)
    ///  64..72  total_file_length     72..104 index_table_hash (32)
    /// 104..108 footer_crc32 (over bytes 0..104)
    /// 108..112 magic "END2"
    /// ```
    pub fn write<W: Write>(&self, w: &mut W) -> Result<()> {
        let mut buf = [0u8; FOOTER_SIZE_V2];
        buf[0..8].copy_from_slice(&self.manifest_offset.to_le_bytes());
        buf[8..16].copy_from_slice(&self.manifest_length.to_le_bytes());
        buf[16..48].copy_from_slice(&self.manifest_hash);
        buf[48..56].copy_from_slice(&self.index_offset.to_le_bytes());
        buf[56..60].copy_from_slice(&self.index_count.to_le_bytes());
        buf[60..64].copy_from_slice(&(FORMAT_VERSION as u32).to_le_bytes());
        buf[64..72].copy_from_slice(&self.total_file_length.to_le_bytes());
        buf[72..104].copy_from_slice(&self.index_table_hash);
        let crc = crc32(&buf[0..104]);
        buf[104..108].copy_from_slice(&crc.to_le_bytes());
        buf[108..112].copy_from_slice(FOOTER_END_MAGIC_V2);
        w.write_all(&buf)?;
        Ok(())
    }

    pub fn read_from_end<R: Read + Seek>(r: &mut R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        // Prefer the v2 footer; fall back to the v1 footer for legacy files.
        if len >= FOOTER_SIZE_V2 as u64 {
            r.seek(SeekFrom::End(-(FOOTER_SIZE_V2 as i64)))?;
            let mut buf = [0u8; FOOTER_SIZE_V2];
            r.read_exact(&mut buf)?;
            if &buf[108..112] == FOOTER_END_MAGIC_V2 {
                let stored_crc = u32::from_le_bytes(buf[104..108].try_into().unwrap());
                let actual_crc = crc32(&buf[0..104]);
                if stored_crc != actual_crc {
                    return Err(PhoenixError::TableCorrupt {
                        what: "footer CRC mismatch (the file's trailer is damaged)".into(),
                    });
                }
                return Ok(Self {
                    manifest_offset: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
                    manifest_length: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
                    manifest_hash: buf[16..48].try_into().unwrap(),
                    index_offset: u64::from_le_bytes(buf[48..56].try_into().unwrap()),
                    index_count: u32::from_le_bytes(buf[56..60].try_into().unwrap()),
                    version: u32::from_le_bytes(buf[60..64].try_into().unwrap()) as u16,
                    total_file_length: u64::from_le_bytes(buf[64..72].try_into().unwrap()),
                    index_table_hash: buf[72..104].try_into().unwrap(),
                });
            }
            // Not a v2 footer; fall through to try v1.
        }
        if len < FOOTER_SIZE as u64 {
            return Err(PhoenixError::InvalidFormat("file too small".into()));
        }
        r.seek(SeekFrom::End(-(FOOTER_SIZE as i64)))?;
        let mut buf = [0u8; FOOTER_SIZE];
        r.read_exact(&mut buf)?;
        if &buf[60..64] != FOOTER_END_MAGIC {
            return Err(PhoenixError::InvalidFormat("bad footer magic".into()));
        }
        Ok(Self {
            manifest_offset: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            manifest_length: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            manifest_hash: buf[16..48].try_into().unwrap(),
            index_offset: u64::from_le_bytes(buf[48..56].try_into().unwrap()),
            index_count: u32::from_le_bytes(buf[56..60].try_into().unwrap()),
            version: 1,
            total_file_length: 0,
            index_table_hash: [0u8; 32],
        })
    }
}

pub fn compress_chunk(data: &[u8]) -> Result<Vec<u8>> {
    zstd::encode_all(data, 3).map_err(|e| PhoenixError::Compression(e.to_string()))
}

pub fn decompress_chunk(data: &[u8], uncompressed_len: usize) -> Result<Vec<u8>> {
    let out = zstd::decode_all(data).map_err(|e| PhoenixError::Compression(e.to_string()))?;
    if out.len() != uncompressed_len {
        return Err(PhoenixError::Compression(format!(
            "expected {uncompressed_len} bytes, got {}",
            out.len()
        )));
    }
    Ok(out)
}

pub struct PhnxWriter {
    file: File,
    header: Header,
    index_entries: Vec<PartitionIndexEntry>,
    current_offset: u64,
    progress: Option<ProgressHandle>,
}

impl PhnxWriter {
    pub fn create(path: &Path, header: Header) -> Result<Self> {
        Self::create_with_progress(path, header, None)
    }

    pub fn create_with_progress(
        path: &Path,
        header: Header,
        progress: Option<ProgressHandle>,
    ) -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        let mut w = Self {
            file,
            header,
            index_entries: Vec::new(),
            current_offset: HEADER_SIZE as u64,
            progress,
        };
        w.header.partition_count = 0;
        w.header.write(&mut w.file)?;
        w.current_offset = HEADER_SIZE as u64 + INDEX_TABLE_RESERVE;
        Ok(w)
    }

    pub fn begin_partition_stream(
        &mut self,
        index: u32,
        type_guid: [u8; 16],
        name: String,
        original_size: u64,
        fs_kind: FilesystemKind,
        capture_mode: CaptureMode,
        sector_size: u32,
        used_bytes: u64,
        extents: &[Extent],
        bytes_per_cluster: u32,
    ) -> Result<PartitionStreamWriter<'_>> {
        let stream_offset = self.current_offset;
        // Map header is 12 bytes (extent count + chunk count + bytes/cluster,
        // three u32s) followed by the extent table (16 bytes per extent). This
        // MUST match the 12-byte header the reader assumes; a previous value of
        // 8 placed `data_start` inside the extent table, so the first chunk
        // overwrote the high 32 bits of the last extent's sector_count.
        let header_size = 12 + extents.len() * 16;
        self.file.seek(SeekFrom::Start(stream_offset))?;
        self.file.write_u32::<LittleEndian>(extents.len() as u32)?;
        self.file.write_u32::<LittleEndian>(0)?; // chunk count placeholder
        self.file.write_u32::<LittleEndian>(bytes_per_cluster)?;
        for e in extents {
            self.file.write_u64::<LittleEndian>(e.start_sector)?;
            self.file.write_u64::<LittleEndian>(e.sector_count)?;
        }
        let data_start = stream_offset + header_size as u64;
        self.current_offset = data_start;

        let entry = PartitionIndexEntry {
            index,
            type_guid,
            name,
            original_size,
            fs_kind,
            capture_mode,
            stream_offset,
            stream_length: 0,
            sector_size,
            used_bytes,
        };
        self.index_entries.push(entry);

        Ok(PartitionStreamWriter {
            writer: self,
            stream_offset,
            data_start,
            extents: extents.to_vec(),
            bytes_per_cluster,
            chunk_indices: Vec::new(),
            chunk_records: Vec::new(),
            current_data_offset: data_start,
            extent_index: 0,
            chunk_in_extent: 0,
        })
    }

    pub fn set_last_partition_used_bytes(&mut self, used_bytes: u64) {
        if let Some(entry) = self.index_entries.last_mut() {
            entry.used_bytes = used_bytes;
        }
    }

    pub fn finalize(mut self, manifest: &BackupManifest) -> Result<()> {
        let manifest_bytes = manifest.to_json()?;
        let manifest_hash = hash::hash_bytes(&manifest_bytes);
        let manifest_offset = self.current_offset;

        self.file.seek(SeekFrom::Start(manifest_offset))?;
        self.file.write_all(&manifest_bytes)?;
        self.current_offset = manifest_offset + manifest_bytes.len() as u64;

        // Serialize the index entries into a buffer so we can both write them
        // and hash the exact bytes for the footer's index_table_hash.
        let index_offset = HEADER_SIZE as u64;
        let mut index_bytes = Vec::with_capacity(self.index_entries.len() * INDEX_ENTRY_SIZE);
        for entry in &self.index_entries {
            entry.write(&mut index_bytes)?;
        }
        let index_table_hash = hash::hash_bytes(&index_bytes);
        self.file.seek(SeekFrom::Start(index_offset))?;
        self.file.write_all(&index_bytes)?;

        self.header.partition_count = self.index_entries.len() as u32;
        self.file.seek(SeekFrom::Start(0))?;
        self.header.write(&mut self.file)?;

        let footer_offset = self.current_offset;
        let footer = Footer {
            manifest_offset,
            manifest_length: manifest_bytes.len() as u64,
            manifest_hash,
            index_offset,
            index_count: self.index_entries.len() as u32,
            version: FORMAT_VERSION,
            total_file_length: footer_offset + FOOTER_SIZE_V2 as u64,
            index_table_hash,
        };
        self.file.seek(SeekFrom::Start(footer_offset))?;
        footer.write(&mut self.file)?;
        self.file.sync_all()?;
        Ok(())
    }
}

pub struct PartitionStreamWriter<'a> {
    writer: &'a mut PhnxWriter,
    stream_offset: u64,
    data_start: u64,
    extents: Vec<Extent>,
    bytes_per_cluster: u32,
    chunk_indices: Vec<ChunkIndex>,
    chunk_records: Vec<ChunkRecord>,
    current_data_offset: u64,
    extent_index: u32,
    chunk_in_extent: u32,
}

impl PartitionStreamWriter<'_> {
    pub fn set_extent(&mut self, extent_index: u32) {
        self.extent_index = extent_index;
        self.chunk_in_extent = 0;
    }

    pub fn write_chunk(&mut self, plaintext: &[u8]) -> Result<()> {
        // Single cancel chokepoint for every capture path. `capture_raw`,
        // `capture_fat`, and `capture_ntfs` all funnel each chunk through
        // here, so a check before we hash + compress aborts the backup at
        // the next chunk boundary regardless of which filesystem driver
        // was used. Latency to honor a Cancel click is bounded by one
        // chunk's worth of read + write.
        if let Some(ref progress) = self.writer.progress {
            if progress.is_cancelled() {
                return Err(PhoenixError::Cancelled);
            }
        }
        let hash_hex = hash::hash_hex(plaintext);
        let compressed = compress_chunk(plaintext)?;
        let file_offset = self.current_data_offset;
        self.writer.file.seek(SeekFrom::Start(file_offset))?;
        self.writer.file.write_all(&compressed)?;
        self.current_data_offset += compressed.len() as u64;

        let chunk_index = self.chunk_indices.len() as u32;
        self.chunk_indices.push(ChunkIndex {
            file_offset,
            compressed_len: compressed.len() as u32,
            uncompressed_len: plaintext.len() as u32,
            extent_index: self.extent_index,
            chunk_index: self.chunk_in_extent,
        });
        self.chunk_records.push(ChunkRecord {
            chunk_index,
            extent_index: self.extent_index,
            uncompressed_len: plaintext.len() as u32,
            blake3: hash_hex,
        });
        self.chunk_in_extent += 1;
        if let Some(ref progress) = self.writer.progress {
            progress.bump(plaintext.len() as u64);
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(Vec<ChunkRecord>, u64)> {
        let stream_length = self.current_data_offset - self.stream_offset;
        let chunk_count = self.chunk_indices.len() as u32;

        // Write chunk index table after compressed data
        let index_table_offset = self.current_data_offset;
        for c in &self.chunk_indices {
            self.writer.file.write_u64::<LittleEndian>(c.file_offset)?;
            self.writer
                .file
                .write_u32::<LittleEndian>(c.compressed_len)?;
            self.writer
                .file
                .write_u32::<LittleEndian>(c.uncompressed_len)?;
            self.writer.file.write_u32::<LittleEndian>(c.extent_index)?;
            self.writer.file.write_u32::<LittleEndian>(c.chunk_index)?;
        }
        let total_stream_len =
            index_table_offset + self.chunk_indices.len() as u64 * 24 - self.stream_offset;

        // Patch chunk count in stream header
        self.writer
            .file
            .seek(SeekFrom::Start(self.stream_offset + 4))?;
        self.writer.file.write_u32::<LittleEndian>(chunk_count)?;

        let idx = self.writer.index_entries.len() - 1;
        self.writer.index_entries[idx].stream_length = total_stream_len;
        self.writer.current_offset = index_table_offset + self.chunk_indices.len() as u64 * 24;

        Ok((self.chunk_records, stream_length))
    }
}

pub struct PhnxReader {
    file: File,
    pub header: Header,
    pub footer: Footer,
    pub index: Vec<PartitionIndexEntry>,
    pub manifest: BackupManifest,
}

impl PhnxReader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let header = Header::read(&mut file)?;
        if header.version < MIN_READ_VERSION || header.version > FORMAT_VERSION {
            return Err(PhoenixError::InvalidFormat(format!(
                "unsupported .phnx format version {} (this build reads {}..={})",
                header.version, MIN_READ_VERSION, FORMAT_VERSION
            )));
        }
        let footer = Footer::read_from_end(&mut file)?;

        // v2: the footer records the exact file length the writer committed.
        // Any truncation or trailing padding is caught here, before any offset
        // in the footer is trusted. (v1 footers carry no length; skip.)
        let actual_len = file.seek(SeekFrom::End(0))?;
        if footer.version >= 2 && footer.total_file_length != actual_len {
            return Err(PhoenixError::Truncated {
                expected: footer.total_file_length,
                actual: actual_len,
            });
        }

        file.seek(SeekFrom::Start(footer.manifest_offset))?;
        let mut manifest_bytes = vec![0u8; footer.manifest_length as usize];
        file.read_exact(&mut manifest_bytes)?;
        let computed = hash::hash_bytes(&manifest_bytes);
        if computed != footer.manifest_hash {
            return Err(PhoenixError::InvalidFormat("manifest hash mismatch".into()));
        }
        let manifest = BackupManifest::from_json(&manifest_bytes)?;

        // Read the index-entry region as raw bytes so we can validate its hash
        // (v2) before parsing any stream offsets out of it.
        file.seek(SeekFrom::Start(footer.index_offset))?;
        let mut index_bytes = vec![0u8; footer.index_count as usize * INDEX_ENTRY_SIZE];
        file.read_exact(&mut index_bytes)?;
        if footer.version >= 2 {
            let computed_idx = hash::hash_bytes(&index_bytes);
            if computed_idx != footer.index_table_hash {
                return Err(PhoenixError::TableCorrupt {
                    what: "partition index table hash mismatch".into(),
                });
            }
        }

        let mut index = Vec::new();
        for i in 0..footer.index_count as usize {
            let raw = &index_bytes[i * INDEX_ENTRY_SIZE..(i + 1) * INDEX_ENTRY_SIZE];
            let entry = PartitionIndexEntry::read(&mut std::io::Cursor::new(raw))?;
            if footer.version >= 2 {
                let stored = PartitionIndexEntry::stored_crc(raw);
                let recomputed = entry.compute_crc()?;
                if stored != recomputed {
                    return Err(PhoenixError::TableCorrupt {
                        what: format!("index entry {} CRC mismatch", entry.index),
                    });
                }
            }
            index.push(entry);
        }

        Ok(Self {
            file,
            header,
            footer,
            index,
            manifest,
        })
    }

    pub fn read_stream_header(&mut self, entry: &PartitionIndexEntry) -> Result<StreamHeader> {
        self.file.seek(SeekFrom::Start(entry.stream_offset))?;
        let map_count = self.file.read_u32::<LittleEndian>()?;
        let chunk_count = self.file.read_u32::<LittleEndian>()?;
        let bytes_per_cluster = self.file.read_u32::<LittleEndian>()?;
        let mut extents = Vec::with_capacity(map_count as usize);
        for _ in 0..map_count {
            extents.push(Extent {
                start_sector: self.file.read_u64::<LittleEndian>()?,
                sector_count: self.file.read_u64::<LittleEndian>()?,
            });
        }
        let map_header_size = 12 + map_count as u64 * 16;
        let data_region_end = entry.stream_offset + entry.stream_length - chunk_count as u64 * 24;
        let mut chunks = Vec::new();
        self.file.seek(SeekFrom::Start(data_region_end))?;
        for _ in 0..chunk_count {
            chunks.push(ChunkIndex {
                file_offset: self.file.read_u64::<LittleEndian>()?,
                compressed_len: self.file.read_u32::<LittleEndian>()?,
                uncompressed_len: self.file.read_u32::<LittleEndian>()?,
                extent_index: self.file.read_u32::<LittleEndian>()?,
                chunk_index: self.file.read_u32::<LittleEndian>()?,
            });
        }
        let _ = map_header_size;
        Ok(StreamHeader {
            extents,
            chunks,
            bytes_per_cluster,
        })
    }

    pub fn read_chunk(&mut self, chunk: &ChunkIndex) -> Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(chunk.file_offset))?;
        let mut compressed = vec![0u8; chunk.compressed_len as usize];
        self.file.read_exact(&mut compressed)?;
        decompress_chunk(&compressed, chunk.uncompressed_len as usize)
    }

    /// Structural integrity check — validates the stream tables of every
    /// partition **without decompressing any chunk data**. Cheap enough to run
    /// even on huge backups, and it catches the whole class of "the metadata
    /// disagrees with itself" corruption:
    ///
    /// * the stream chunk table and manifest chunk records must have the same
    ///   length (missing / extra chunks);
    /// * every chunk's byte range must fall inside the partition's data region
    ///   (not into the tables, manifest, or off the end of the file);
    /// * every chunk's `extent_index` must be in range;
    /// * the chunks of each extent must exactly cover the extent's byte length;
    /// * extents must not overlap.
    ///
    /// Combined with the open-time checks (header CRC, footer CRC + total
    /// length, manifest hash, index-table hash) this makes a "quick" verify
    /// meaningful: truncation and metadata corruption are always detected.
    pub fn verify_structure(&mut self) -> Result<StructureReport> {
        let entries: Vec<PartitionIndexEntry> = self.index.clone();
        let data_upper_bound = self.footer.manifest_offset;
        let mut total_chunks = 0u64;
        let mut total_bytes = 0u64;

        for entry in &entries {
            let stream = self.read_stream_header(entry)?;
            let records: Vec<ChunkRecord> = self
                .manifest
                .partitions
                .iter()
                .find(|p| p.index == entry.index)
                .map(|p| p.chunks.clone())
                .ok_or_else(|| PhoenixError::Manifest("partition manifest missing".into()))?;

            if stream.chunks.len() != records.len() {
                return Err(PhoenixError::ChunkCountMismatch {
                    partition_index: entry.index,
                    stream_chunks: stream.chunks.len(),
                    manifest_chunks: records.len(),
                });
            }

            let map_header_size = 12 + stream.extents.len() as u64 * 16;
            let data_start = entry.stream_offset + map_header_size;
            let index_table_offset =
                entry.stream_offset + entry.stream_length - stream.chunks.len() as u64 * 24;

            // Per-extent coverage accumulator.
            let mut covered = vec![0u64; stream.extents.len()];
            for chunk in &stream.chunks {
                let ei = chunk.extent_index as usize;
                if ei >= stream.extents.len() {
                    return Err(PhoenixError::TableCorrupt {
                        what: format!(
                            "partition {}: chunk references extent {} of {}",
                            entry.index,
                            ei,
                            stream.extents.len()
                        ),
                    });
                }
                let end = chunk.file_offset + chunk.compressed_len as u64;
                if chunk.file_offset < data_start
                    || end > index_table_offset
                    || end > data_upper_bound
                {
                    return Err(PhoenixError::TableCorrupt {
                        what: format!(
                            "partition {}: chunk byte range {}..{} lies outside its data region \
                             {}..{}",
                            entry.index, chunk.file_offset, end, data_start, index_table_offset
                        ),
                    });
                }
                covered[ei] += chunk.uncompressed_len as u64;
                total_chunks += 1;
                total_bytes += chunk.uncompressed_len as u64;
            }

            for (i, ext) in stream.extents.iter().enumerate() {
                let expected = ext.sector_count * EXTENT_LBA_BYTES as u64;
                if covered[i] != expected {
                    return Err(PhoenixError::TableCorrupt {
                        what: format!(
                            "partition {}: extent {} spans {} bytes but its chunks cover {}",
                            entry.index, i, expected, covered[i]
                        ),
                    });
                }
            }

            // Extents must not overlap (sort by start, check adjacency).
            let mut spans: Vec<(u64, u64)> = stream
                .extents
                .iter()
                .map(|e| {
                    (
                        e.start_sector,
                        e.start_sector.saturating_add(e.sector_count),
                    )
                })
                .collect();
            spans.sort_by_key(|s| s.0);
            for w in spans.windows(2) {
                if w[0].1 > w[1].0 {
                    return Err(PhoenixError::TableCorrupt {
                        what: format!("partition {}: overlapping extents", entry.index),
                    });
                }
            }
        }

        Ok(StructureReport {
            partitions: entries.len(),
            total_chunks,
            total_bytes,
        })
    }

    /// Decompress + BLAKE3-check a deterministic sample of chunks per
    /// partition (always including the first and last). Fast spot-check for
    /// the "quick" tier; `seed` makes the selection reproducible.
    pub fn verify_sampled(&mut self, per_partition: usize, seed: u64) -> Result<()> {
        let entries: Vec<PartitionIndexEntry> = self.index.clone();
        for entry in &entries {
            let stream = self.read_stream_header(entry)?;
            let records: Vec<ChunkRecord> = self
                .manifest
                .partitions
                .iter()
                .find(|p| p.index == entry.index)
                .map(|p| p.chunks.clone())
                .ok_or_else(|| PhoenixError::Manifest("partition manifest missing".into()))?;
            let paired: Vec<_> = paired_chunks(&stream.chunks, &records, entry.index)?.collect();
            if paired.is_empty() {
                continue;
            }
            for idx in sample_indices(paired.len(), per_partition, seed ^ entry.index as u64) {
                let (chunk, record) = paired[idx];
                let data = self.read_chunk(chunk)?;
                if hash::hash_hex(&data) != record.blake3 {
                    return Err(PhoenixError::HashMismatch {
                        partition_index: entry.index,
                        chunk_index: record.chunk_index,
                    });
                }
            }
        }
        Ok(())
    }

    pub fn verify_partition(&mut self, partition_index: u32, quick: bool) -> Result<()> {
        let entry = self
            .index
            .iter()
            .find(|e| e.index == partition_index)
            .cloned()
            .ok_or_else(|| PhoenixError::InvalidFormat("partition not found".into()))?;
        let stream = self.read_stream_header(&entry)?;
        let chunk_records: Vec<ChunkRecord> = self
            .manifest
            .partitions
            .iter()
            .find(|p| p.index == partition_index)
            .map(|p| p.chunks.clone())
            .ok_or_else(|| PhoenixError::Manifest("partition manifest missing".into()))?;

        if quick {
            // Quick: structural check for this partition + a hashed sample.
            // (verify_structure walks all partitions; for a single-partition
            // quick check we do the count + sample here.)
            if stream.chunks.len() != chunk_records.len() {
                return Err(PhoenixError::ChunkCountMismatch {
                    partition_index,
                    stream_chunks: stream.chunks.len(),
                    manifest_chunks: chunk_records.len(),
                });
            }
            return self.verify_sampled(16, 0);
        }

        for (chunk, record) in paired_chunks(&stream.chunks, &chunk_records, partition_index)? {
            let data = self.read_chunk(chunk)?;
            let computed = hash::hash_hex(&data);
            if computed != record.blake3 {
                return Err(PhoenixError::HashMismatch {
                    partition_index,
                    chunk_index: record.chunk_index,
                });
            }
        }
        Ok(())
    }

    pub fn verify_all(&mut self, quick: bool) -> Result<()> {
        self.verify_all_with_progress(quick, None)
    }

    pub fn verify_all_with_progress(
        &mut self,
        quick: bool,
        progress: Option<ProgressHandle>,
    ) -> Result<()> {
        if quick {
            // Quick tier: structural integrity of every partition (no
            // decompression) plus a hashed spot-check sample. Fast even on
            // multi-terabyte backups, but now genuinely meaningful — it catches
            // truncation (via the open-time length check), metadata corruption,
            // chunk-count mismatches, and a statistical sample of bit-rot.
            if let Some(ref p) = progress {
                p.set_steps(vec![
                    "Structure check".to_string(),
                    "Sampling chunks".to_string(),
                ]);
                p.begin(2, "Quick verify");
                p.set_step(0);
                p.set(1, "Structure check");
            }
            self.verify_structure()?;
            if let Some(ref p) = progress {
                p.set_step(1);
                p.set(2, "Sampling chunks");
            }
            self.verify_sampled(16, 0)?;
            if let Some(ref p) = progress {
                p.end();
            }
            return Ok(());
        }

        // Full tier: structural check first (cheap, gives a clear error before
        // we spend time decompressing), then hash every chunk.
        self.verify_structure()?;

        let total_chunks: u64 = self
            .manifest
            .partitions
            .iter()
            .map(|p| p.chunks.len() as u64)
            .sum();

        // Declare one step per partition up front so the GUI modal can show
        // upcoming partitions grayed out.
        let indices: Vec<u32> = self.index.iter().map(|e| e.index).collect();
        if let Some(ref p) = progress {
            let steps: Vec<String> = indices
                .iter()
                .map(|idx| {
                    let name = self
                        .index
                        .iter()
                        .find(|e| e.index == *idx)
                        .map(|e| e.name.as_str())
                        .unwrap_or("partition");
                    format!("Verifying {name}")
                })
                .collect();
            p.set_steps(steps);
            p.begin(total_chunks.max(1), "Verify");
        }

        let mut done = 0u64;
        for (step, idx) in indices.into_iter().enumerate() {
            if let Some(ref p) = progress {
                p.set_step(step);
            }
            done += self.verify_partition_with_progress(idx, progress.as_ref(), done)?;
        }

        if let Some(ref p) = progress {
            p.end();
        }
        Ok(())
    }

    fn verify_partition_with_progress(
        &mut self,
        partition_index: u32,
        progress: Option<&ProgressHandle>,
        mut done: u64,
    ) -> Result<u64> {
        let entry = self
            .index
            .iter()
            .find(|e| e.index == partition_index)
            .cloned()
            .ok_or_else(|| PhoenixError::InvalidFormat("partition not found".into()))?;

        let stream = self.read_stream_header(&entry)?;
        let chunk_records: Vec<ChunkRecord> = self
            .manifest
            .partitions
            .iter()
            .find(|p| p.index == partition_index)
            .map(|p| p.chunks.clone())
            .ok_or_else(|| PhoenixError::Manifest("partition manifest missing".into()))?;

        for (chunk, record) in paired_chunks(&stream.chunks, &chunk_records, partition_index)? {
            if let Some(p) = progress {
                if p.is_cancelled() {
                    return Err(PhoenixError::Cancelled);
                }
            }
            let data = self.read_chunk(chunk)?;
            let computed = hash::hash_hex(&data);
            if computed != record.blake3 {
                return Err(PhoenixError::HashMismatch {
                    partition_index,
                    chunk_index: record.chunk_index,
                });
            }
            done += 1;
            if let Some(p) = progress {
                p.set(done, format!("Chunk {} / {}", done, p.snapshot().total));
            }
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{BackupManifest, DiskManifest, PartitionManifest};
    use std::io::Cursor;
    use tempfile::NamedTempFile;

    #[test]
    fn header_roundtrip() {
        let h = Header {
            version: FORMAT_VERSION,
            flags: 1,
            timestamp: 1_700_000_000,
            backup_id: Uuid::new_v4(),
            disk_signature: 42,
            partition_count: 0,
        };
        let mut buf = Vec::new();
        h.write(&mut buf).unwrap();
        let mut cur = Cursor::new(buf);
        let h2 = Header::read(&mut cur).unwrap();
        assert_eq!(h2.backup_id, h.backup_id);
    }

    #[test]
    fn compress_roundtrip() {
        let data = vec![0xABu8; 1000];
        let c = compress_chunk(&data).unwrap();
        let d = decompress_chunk(&c, 1000).unwrap();
        assert_eq!(d, data);
    }

    #[test]
    fn stream_extents_survive_roundtrip() {
        // Regression: the map header is 12 bytes, so the first chunk must not
        // overlap the extent table. Previously data_start was computed 4 bytes
        // early and the first chunk clobbered the high 32 bits of the LAST
        // extent's sector_count.
        use crate::manifest::{BackupManifest, DiskManifest, PartitionManifest};
        let path = std::env::temp_dir().join(format!("extents_{}.phnx", Uuid::new_v4()));
        let header = Header {
            version: FORMAT_VERSION,
            flags: 0,
            timestamp: 1,
            backup_id: Uuid::new_v4(),
            disk_signature: 1,
            partition_count: 1,
        };
        let extents = vec![
            Extent {
                start_sector: 0,
                sector_count: 8,
            },
            Extent {
                start_sector: 100,
                sector_count: 50,
            },
        ];
        let mut writer = PhnxWriter::create(&path, header).unwrap();
        let mut stream = writer
            .begin_partition_stream(
                0,
                [0; 16],
                "T".into(),
                4096,
                FilesystemKind::Ntfs,
                CaptureMode::UsedBlocks,
                EXTENT_LBA_BYTES,
                0,
                &extents,
                4096,
            )
            .unwrap();
        stream.set_extent(0);
        stream.write_chunk(&[0xCD; 4096]).unwrap();
        stream.set_extent(1);
        stream.write_chunk(&[0xEF; 4096]).unwrap();
        let (chunks, _) = stream.finish().unwrap();
        let manifest = BackupManifest {
            format_version: 1,
            backup_id: writer.header.backup_id,
            parent_backup_id: None,
            hostname: "T".into(),
            disk: DiskManifest {
                style: "gpt".into(),
                disk_guid: None,
                sector_size: 512,
            },
            partitions: vec![PartitionManifest {
                index: 0,
                name: "T".into(),
                type_guid: None,
                fs: "ntfs".into(),
                capture_mode: "used-blocks".into(),
                original_size: 4096,
                used_bytes: 8192,
                bitlocker: None,
                chunks,
                bitmap_hash: None,
            }],
        };
        writer.finalize(&manifest).unwrap();

        let mut reader = PhnxReader::open(&path).unwrap();
        let entry = reader.index[0].clone();
        let sh = reader.read_stream_header(&entry).unwrap();
        assert_eq!(sh.extents.len(), 2);
        assert_eq!(sh.extents[0].start_sector, 0);
        assert_eq!(sh.extents[0].sector_count, 8);
        assert_eq!(sh.extents[1].start_sector, 100);
        assert_eq!(
            sh.extents[1].sector_count, 50,
            "last extent sector_count clobbered"
        );
        let _ = std::fs::remove_file(&path);
    }

    fn dummy_chunk(i: u32) -> ChunkIndex {
        ChunkIndex {
            file_offset: 0,
            compressed_len: 0,
            uncompressed_len: 0,
            extent_index: 0,
            chunk_index: i,
        }
    }

    fn dummy_record(i: u32) -> crate::manifest::ChunkRecord {
        crate::manifest::ChunkRecord {
            chunk_index: i,
            extent_index: 0,
            uncompressed_len: 0,
            blake3: String::new(),
        }
    }

    #[test]
    fn footer_reads_v1_and_v2() {
        // v2 footer roundtrips and reports version 2.
        let f = Footer {
            manifest_offset: 100,
            manifest_length: 50,
            manifest_hash: [7u8; 32],
            index_offset: 64,
            index_count: 2,
            version: FORMAT_VERSION,
            total_file_length: 1234,
            index_table_hash: [9u8; 32],
        };
        let mut buf = vec![0u8; 500];
        {
            let mut cur = Cursor::new(&mut buf);
            cur.seek(SeekFrom::End(-(FOOTER_SIZE_V2 as i64))).unwrap();
            f.write(&mut cur).unwrap();
        }
        let mut cur = Cursor::new(buf);
        let read = Footer::read_from_end(&mut cur).unwrap();
        assert_eq!(read.version, 2);
        assert_eq!(read.total_file_length, 1234);
        assert_eq!(read.manifest_offset, 100);

        // A hand-built v1 footer (72 bytes, END\0) is still parsed, with the
        // v2-only fields defaulted and version = 1.
        let mut v1 = vec![0xEEu8; 300];
        let start = v1.len() - FOOTER_SIZE;
        v1[start..start + 8].copy_from_slice(&200u64.to_le_bytes()); // manifest_offset
        v1[start + 8..start + 16].copy_from_slice(&40u64.to_le_bytes()); // manifest_length
        v1[start + 16..start + 48].copy_from_slice(&[3u8; 32]); // manifest_hash
        v1[start + 48..start + 56].copy_from_slice(&64u64.to_le_bytes()); // index_offset
        v1[start + 56..start + 60].copy_from_slice(&1u32.to_le_bytes()); // index_count
        v1[start + 60..start + 64].copy_from_slice(b"END\x00"); // v1 magic
        v1[start + 64..start + 72].fill(0); // reserved
        let mut cur = Cursor::new(v1);
        let read = Footer::read_from_end(&mut cur).unwrap();
        assert_eq!(read.version, 1);
        assert_eq!(read.total_file_length, 0);
        assert_eq!(read.manifest_offset, 200);
        assert_eq!(read.index_count, 1);
    }

    #[test]
    fn paired_chunks_matches_equal_lengths() {
        let chunks = vec![dummy_chunk(0), dummy_chunk(1)];
        let records = vec![dummy_record(0), dummy_record(1)];
        let paired: Vec<_> = paired_chunks(&chunks, &records, 3).unwrap().collect();
        assert_eq!(paired.len(), 2);
    }

    #[test]
    fn paired_chunks_rejects_length_mismatch() {
        let chunks = vec![dummy_chunk(0), dummy_chunk(1), dummy_chunk(2)];
        let records = vec![dummy_record(0), dummy_record(1)];
        let err = paired_chunks(&chunks, &records, 3).unwrap_err();
        match err {
            PhoenixError::ChunkCountMismatch {
                partition_index,
                stream_chunks,
                manifest_chunks,
            } => {
                assert_eq!(partition_index, 3);
                assert_eq!(stream_chunks, 3);
                assert_eq!(manifest_chunks, 2);
            }
            other => panic!("expected ChunkCountMismatch, got {other:?}"),
        }
    }
}
