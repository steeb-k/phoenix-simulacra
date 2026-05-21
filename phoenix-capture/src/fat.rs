use byteorder::{LittleEndian, ReadBytesExt};
use phoenix_core::container::{Extent, CHUNK_SIZE};
use phoenix_core::error::{PhoenixError, Result};
use phoenix_core::hash;
use phoenix_core::ProgressHandle;

use crate::reader::PartitionReader;

const SECTOR: u64 = 512;

#[derive(Clone, Copy, PartialEq)]
enum FatType {
    Fat16,
    Fat32,
    Exfat,
}

pub fn capture_fat(
    reader: &mut PartitionReader,
    stream: &mut phoenix_core::container::PartitionStreamWriter<'_>,
    exfat: bool,
) -> Result<(u64, Option<String>)> {
    let mut boot = vec![0u8; 512];
    reader.read_at(0, &mut boot)?;
    let (fat_type, cluster_size, data_start, total_clusters, fat_bits) =
        parse_fat_boot(&boot, exfat)?;

    let fat_len = ((total_clusters + 2) * fat_bits as u64 + 7) / 8;
    let mut fat_table = vec![0u8; fat_len as usize];
    reader.read_at(data_start, &mut fat_table)?;

    let bitmap_hash = Some(hash::hash_hex(&fat_table));
    let extents = fat_used_extents(&fat_table, fat_type, cluster_size, data_start, total_clusters);

    let mut total_used = 0u64;
    for (ext_idx, extent) in extents.iter().enumerate() {
        stream.set_extent(ext_idx as u32);
        let byte_len = extent.sector_count * SECTOR;
        let base = extent.start_sector * SECTOR;
        let mut pos = 0u64;
        while pos < byte_len {
            let to_read = CHUNK_SIZE.min((byte_len - pos) as usize);
            let mut buf = vec![0u8; to_read];
            let n = reader.read_at(base + pos, &mut buf)?;
            if n == 0 {
                break;
            }
            stream.write_chunk(&buf[..n])?;
            total_used += n as u64;
            pos += n as u64;
        }
    }

    Ok((total_used, bitmap_hash))
}

fn parse_fat_boot(boot: &[u8], exfat: bool) -> Result<(FatType, u64, u64, u64, u32)> {
    if exfat {
        if boot.len() < 512 || boot[3] != 0x45 {
            return Err(PhoenixError::Other("not exFAT".into()));
        }
        let cluster_size = 1u64 << boot[108];
        let cluster_count = u32::from_le_bytes(boot[116..120].try_into().unwrap()) as u64;
        let fat_offset = u32::from_le_bytes(boot[80..84].try_into().unwrap()) as u64 * SECTOR;
        return Ok((
            FatType::Exfat,
            cluster_size,
            fat_offset + cluster_count * cluster_size,
            cluster_count,
            32,
        ));
    }

    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(PhoenixError::Other("invalid FAT boot signature".into()));
    }
    let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]) as u64;
    let sectors_per_cluster = boot[13] as u64;
    let cluster_size = bytes_per_sector * sectors_per_cluster;
    let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u64;
    let fat_count = boot[16] as u64;
    let fat_size_sectors = u16::from_le_bytes([boot[22], boot[23]]) as u64;
    let total_sectors = if boot[19] != 0 {
        u16::from_le_bytes([boot[19], boot[20]]) as u64
    } else {
        u32::from_le_bytes(boot[32..36].try_into().unwrap()) as u64
    };
    let fat_type = if fat_size_sectors == 0 {
        let fat_size32 = u32::from_le_bytes(boot[36..40].try_into().unwrap()) as u64;
        let root_clusters = u32::from_le_bytes(boot[44..48].try_into().unwrap()) as u64;
        let data_start =
            (reserved + fat_count * fat_size32) * bytes_per_sector;
        let cluster_count = root_clusters;
        (FatType::Fat32, cluster_size, data_start, cluster_count, 32)
    } else {
        let data_start = (reserved + fat_count * fat_size_sectors) * bytes_per_sector;
        let cluster_count = (total_sectors - reserved - fat_count * fat_size_sectors)
            / sectors_per_cluster;
        (FatType::Fat16, cluster_size, data_start, cluster_count, 16)
    };
    Ok(fat_type)
}

fn fat_used_extents(
    fat: &[u8],
    fat_type: FatType,
    cluster_size: u64,
    data_start: u64,
    total_clusters: u64,
) -> Vec<Extent> {
    let mut used = Vec::new();
    for c in 2..=total_clusters + 1 {
        let entry = fat_cluster_value(fat, c, fat_type);
        if entry != 0 && entry != 0x0FFF_FFFF && entry != 0xFFFF {
            used.push(c);
        }
    }
    if used.is_empty() {
        return vec![Extent {
            start_sector: 0,
            sector_count: (cluster_size / SECTOR).max(8),
        }];
    }

    let mut extents = Vec::new();
    let mut start = used[0];
    let mut prev = used[0];
    for &c in used.iter().skip(1) {
        if c == prev + 1 {
            prev = c;
        } else {
            let byte_start = data_start + (start - 2) * cluster_size;
            extents.push(Extent {
                start_sector: byte_start / SECTOR,
                sector_count: ((prev - start + 1) * cluster_size) / SECTOR,
            });
            start = c;
            prev = c;
        }
    }
    let byte_start = data_start + (start - 2) * cluster_size;
    extents.push(Extent {
        start_sector: byte_start / SECTOR,
        sector_count: ((prev - start + 1) * cluster_size) / SECTOR,
    });
    extents
}

fn fat_cluster_value(fat: &[u8], cluster: u64, fat_type: FatType) -> u32 {
    match fat_type {
        FatType::Fat16 => {
            let off = (cluster * 2) as usize;
            if off + 2 > fat.len() {
                return 0;
            }
            u16::from_le_bytes([fat[off], fat[off + 1]]) as u32
        }
        FatType::Fat32 | FatType::Exfat => {
            let off = (cluster * 4) as usize;
            if off + 4 > fat.len() {
                return 0;
            }
            u32::from_le_bytes(fat[off..off + 4].try_into().unwrap()) & 0x0FFF_FFFF
        }
    }
}

pub fn capture_exfat(
    reader: &mut PartitionReader,
    stream: &mut phoenix_core::container::PartitionStreamWriter<'_>,
) -> Result<(u64, Option<String>)> {
    capture_fat(reader, stream, true)
}

pub fn estimate_fat_used(reader: &mut PartitionReader, exfat: bool) -> Result<u64> {
    let mut boot = vec![0u8; 512];
    reader.read_at(0, &mut boot)?;
    let (_, cluster_size, data_start, total_clusters, _) = parse_fat_boot(&boot, exfat)?;
    let fat_bits = if exfat { 32 } else { 32 };
    let fat_len = ((total_clusters + 2) * fat_bits as u64 + 7) / 8;
    let mut fat_table = vec![0u8; fat_len as usize];
    reader.read_at(data_start, &mut fat_table)?;
    let extents = fat_used_extents(&fat_table, FatType::Fat32, cluster_size, data_start, total_clusters);
    Ok(extents.iter().map(|e| e.sector_count * SECTOR).sum())
}

pub fn restore_fat(
    reader: &mut phoenix_core::container::PhnxReader,
    entry: &phoenix_core::container::PartitionIndexEntry,
    writer: &mut crate::raw::PartitionWriter,
    target_size: u64,
    verify: bool,
    progress: Option<&ProgressHandle>,
    chunks_done: u64,
) -> Result<u64> {
    if entry.used_bytes > target_size {
        return Err(PhoenixError::PartitionTooSmall {
            partition_index: entry.index,
            target_size,
            required: entry.used_bytes,
        });
    }
    crate::raw::restore_raw(reader, entry, writer, verify, progress, chunks_done)
}
