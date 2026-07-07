# Carbon Phoenix `.phnx` Backup Format (v1)

Single-file container for disk/partition backups. All multi-byte integers are **little-endian**.

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
| 4 | 2 | Format version (1) |
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

## Footer (72 bytes)

| Offset | Size | Field |
|--------|------|-------|
| 0 | 8 | Manifest file offset |
| 8 | 8 | Manifest length |
| 16 | 32 | BLAKE3 hash of manifest bytes |
| 48 | 8 | Index table offset |
| 56 | 4 | Index entry count |
| 60 | 4 | Footer magic `END\0` (bytes `45 4E 44 00`) |
| 64 | 8 | Reserved |

## Integrity

1. **Per-chunk BLAKE3** of uncompressed plaintext (stored in manifest).
2. **Manifest root**: BLAKE3 of entire manifest JSON (stored in footer).
3. **Header CRC32**: IEEE CRC over header with CRC field zeroed.

## Incremental (future)

`parent_backup_id` links to prior backup. `bitmap_hash` stores BLAKE3 of used-cluster bitmap for delta detection.
