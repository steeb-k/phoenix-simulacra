# Carbon Phoenix `.phnx` Backup Format (v2)

Single-file container for disk/partition backups. All multi-byte integers are **little-endian**.

The current writer emits **version 2**. Version 1 files still open (see the
[v1 appendix](#appendix-v1-differences)); the reader accepts versions 1–2.
v2 adds full metadata integrity: a footer CRC, a stored total-file-length
(truncation detection), a BLAKE3 over the partition index table, and a per-entry
CRC — so a "quick" verify is meaningful and truncation is always caught.

## File Layout

```
┌─────────────────────────────────────────────────────────────┐
│ Header (64 bytes, fixed)                                    │
├─────────────────────────────────────────────────────────────┤
│ Partition index table (N × 160 bytes)                       │
├─────────────────────────────────────────────────────────────┤
│ Partition stream 0 (sparse map + zstd chunks)               │
│ Partition stream 1 ...                                      │
├─────────────────────────────────────────────────────────────┤
│ Manifest blob (JSON, UTF-8)                                 │
├─────────────────────────────────────────────────────────────┤
│ Footer (72 bytes, fixed)                                    │
└─────────────────────────────────────────────────────────────┘
```

## Header (64 bytes)

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | Magic `PHNX` |
| 4 | 2 | Format version (2; reader accepts 1–2) |
| 6 | 2 | Flags (bit0: GPT, bit1: incremental-capable) |
| 8 | 8 | Backup timestamp (Unix seconds) |
| 16 | 16 | Backup UUID |
| 32 | 8 | Source disk signature (GPT disk GUID or MBR checksum) |
| 40 | 4 | Partition count |
| 44 | 4 | Header CRC32 (bytes 0–43, zeroed when computing) |
| 48 | 16 | Reserved |

## Partition Index Entry (160 bytes each)

| Offset | Size | Field |
|--------|------|-------|
| 0 | 4 | Partition index (source order) |
| 4 | 16 | Partition type GUID (GPT) or type byte + padding |
| 20 | 72 | Partition name (UTF-16LE, up to 36 code units, NUL-terminated) |
| 92 | 20 | Reserved |
| 112 | 8 | Original size in bytes |
| 120 | 1 | Filesystem kind (see enum) |
| 121 | 1 | Capture mode (0=raw, 1=used-blocks) |
| 122 | 2 | Reserved |
| 124 | 8 | Stream offset in file |
| 132 | 8 | Stream length |
| 140 | 4 | Extent addressing unit ("sector size"), always **512** |
| 144 | 8 | Used bytes (logical data size) |
| 152 | 8 | Reserved |

The field at offset 140 is a fixed 512-byte **extent addressing unit**, not
the source disk's physical sector size. Every extent `start_sector` /
`sector_count` in the streams is expressed in these 512-byte units and
restore multiplies by this value to recover byte offsets, so it is 512 even
for backups taken from 4Kn disks. The disk's real logical sector size is
recorded in the manifest's `disk.sector_size`.

### FilesystemKind (u8)

- `0` Unknown / raw
- `1` NTFS
- `2` FAT12/16/32
- `3` exFAT
- `4` EFI System
- `5` MSR
- `6` BitLocker (captured raw)

## Partition Stream

Each stream begins with a **sparse map header**:

| Field | Type | Description |
|-------|------|-------------|
| `map_entry_count` | u32 | Number of extent entries |
| `chunk_count` | u32 | Number of compressed chunks |
| `bytes_per_cluster` | u32 | Cluster size (0 for raw sector mode) |

Followed by `map_entry_count` × **Extent** (16 bytes):

| Field | Type |
|-------|------|
| `start_sector` | u64 |
| `sector_count` | u64 |

Followed by `chunk_count` × **Chunk index** (24 bytes):

| Field | Type |
|-------|------|
| `file_offset` | u64 | Offset of zstd blob in stream |
| `compressed_len` | u32 |
| `uncompressed_len` | u32 |
| `extent_index` | u32 | Which extent this chunk belongs to |
| `chunk_index` | u32 | Sequential index within extent |

Then concatenated **zstd-compressed** blobs (default 4 MiB uncompressed per chunk).

## Manifest (JSON)

Stored at `footer.manifest_offset`, length `footer.manifest_length`.

```json
{
  "format_version": 1,
  "backup_id": "uuid",
  "parent_backup_id": null,
  "hostname": "HOST",
  "disk": { "style": "gpt", "disk_guid": "...", "sector_size": 512 },
  "partitions": [
    {
      "index": 0,
      "name": "EFI",
      "type_guid": "...",
      "fs": "efi",
      "capture_mode": "raw",
      "original_size": 104857600,
      "used_bytes": 25000000,
      "chunks": [
        {
          "chunk_index": 0,
          "extent_index": 0,
          "uncompressed_len": 4194304,
          "blake3": "hex..."
        }
      ],
      "bitmap_hash": null
    }
  ]
}
```

## Footer (v2, 112 bytes)

| Offset | Size | Field |
|--------|------|-------|
| 0 | 8 | Manifest file offset |
| 8 | 8 | Manifest length |
| 16 | 32 | BLAKE3 hash of manifest bytes |
| 48 | 8 | Index table offset |
| 56 | 4 | Index entry count |
| 60 | 4 | Format version (u32, = 2) |
| 64 | 8 | **Total file length** (bytes; the file must be exactly this long) |
| 72 | 32 | **BLAKE3 of the partition index-entry region** |
| 104 | 4 | **Footer CRC32** (IEEE, over footer bytes 0–103) |
| 108 | 4 | Footer magic `END2` (bytes `45 4E 44 32`) |

## Integrity (v2)

Every metadata structure is covered by a checksum, and the reader validates
them before trusting any offset:

| Structure | Protected by | Checked at |
|-----------|--------------|-----------|
| Header | Header CRC32 | open |
| Footer | Footer CRC32 | open |
| Whole file length | `total_file_length` in footer | open (truncation/padding) |
| Partition index table | BLAKE3 in footer | open |
| Each index entry | CRC32 at entry offset 152 | open |
| Manifest JSON | BLAKE3 in footer | open |
| Per chunk (plaintext) | BLAKE3 in manifest | full/sampled verify |
| Stream extent & chunk tables | structural coverage math (`verify_structure`) | verify |

### Verify tiers

- **Open** always checks the header/footer CRCs, the total length, the manifest
  hash, the index-table hash, and every index-entry CRC.
- **Quick** (`verify --quick`) additionally runs `verify_structure` (chunk-count
  equality, chunk byte-ranges in bounds, per-extent coverage, no extent overlap)
  and decompresses + BLAKE3-checks a deterministic sample of chunks.
- **Full** (`verify`) runs the structure check and then BLAKE3-checks every chunk.

## Appendix: v1 differences

Version 1 files use a **72-byte footer** with magic `END\0` (bytes
`45 4E 44 00`) and fields: manifest offset (0), manifest length (8), manifest
BLAKE3 (16), index offset (48), index count (56), magic (60), reserved (64).
They carry no total-length, index-table hash, footer CRC, or per-entry CRC, so
for v1 the open-time checks are limited to the header CRC and manifest hash, and
`verify_structure` reports table checksums as unavailable. v1 index entries have
zero in the CRC slot (offset 152). Everything else (header, index-entry layout,
stream format, manifest) is identical across v1 and v2.

## Incremental (future)

`parent_backup_id` links to prior backup. `bitmap_hash` stores BLAKE3 of used-cluster bitmap for delta detection.
