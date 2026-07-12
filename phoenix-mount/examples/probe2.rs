//! Deep-dive: dump extent/chunk structure of a partition around a failing
//! byte offset, and check global invariants the mount's chunkstore assumes.
//!
//! Usage: probe2 <backup.phnx> <partition_index> <partition_byte>

use std::collections::HashMap;

use phoenix_core::container::{PhnxReader, CHUNK_SIZE, EXTENT_LBA_BYTES};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: probe2 <phnx> <part> <byte>");
    let part: u32 = args.next().unwrap().parse().unwrap();
    let target: u64 = args.next().unwrap().parse().unwrap();

    let mut reader = PhnxReader::open(std::path::Path::new(&path)).unwrap();
    let entry = reader
        .index
        .iter()
        .find(|e| e.index == part)
        .cloned()
        .unwrap();
    let stream = reader.read_stream_header(&entry).unwrap();
    let records = reader
        .manifest
        .partitions
        .iter()
        .find(|p| p.index == part)
        .map(|p| p.chunks.clone())
        .unwrap();

    println!(
        "partition {}: {} extents, {} stream chunks, {} manifest records",
        part,
        stream.extents.len(),
        stream.chunks.len(),
        records.len()
    );

    // Invariant: no duplicate (extent_index, chunk_index) in either table.
    let mut seen = HashMap::new();
    for c in &stream.chunks {
        *seen.entry((c.extent_index, c.chunk_index)).or_insert(0u32) += 1;
    }
    let dups: Vec<_> = seen.iter().filter(|(_, &n)| n > 1).collect();
    println!("stream duplicate (extent,chunk) keys: {}", dups.len());
    let mut seen_m = HashMap::new();
    for r in &records {
        *seen_m.entry((r.extent_index, r.chunk_index)).or_insert(0u32) += 1;
    }
    let dups_m: Vec<_> = seen_m.iter().filter(|(_, &n)| n > 1).collect();
    println!("manifest duplicate (extent,chunk) keys: {}", dups_m.len());

    // Invariant: extents sorted / non-overlapping?
    let mut sorted = stream.extents.clone();
    sorted.sort_by_key(|e| e.start_sector);
    let mut overlaps = 0;
    for w in sorted.windows(2) {
        if w[0].start_sector + w[0].sector_count > w[1].start_sector {
            overlaps += 1;
            if overlaps <= 5 {
                println!(
                    "  OVERLAP: extent [{}..{}) then [{}..{}) (sectors)",
                    w[0].start_sector,
                    w[0].start_sector + w[0].sector_count,
                    w[1].start_sector,
                    w[1].start_sector + w[1].sector_count
                );
            }
        }
    }
    println!("overlapping extent pairs: {overlaps}");

    // Per-extent: sum of chunk lens vs extent len; short non-final chunks.
    let mut by_extent: HashMap<u32, Vec<_>> = HashMap::new();
    for c in &stream.chunks {
        by_extent.entry(c.extent_index).or_default().push(c.clone());
    }
    let mut bad_sum = 0;
    let mut short_mid = 0;
    for (ei, ext) in stream.extents.iter().enumerate() {
        let mut chunks = by_extent.get(&(ei as u32)).cloned().unwrap_or_default();
        chunks.sort_by_key(|c| c.chunk_index);
        let sum: u64 = chunks.iter().map(|c| c.uncompressed_len as u64).sum();
        let ext_len = ext.sector_count * EXTENT_LBA_BYTES as u64;
        if sum != ext_len {
            bad_sum += 1;
            if bad_sum <= 5 {
                println!(
                    "  extent {ei}: chunk len sum {sum} != extent len {ext_len} ({} chunks)",
                    chunks.len()
                );
            }
        }
        for (i, c) in chunks.iter().enumerate() {
            if i + 1 < chunks.len() && (c.uncompressed_len as usize) != CHUNK_SIZE {
                short_mid += 1;
                if short_mid <= 5 {
                    println!(
                        "  extent {ei}: NON-FINAL short chunk idx {} len {}",
                        c.chunk_index, c.uncompressed_len
                    );
                }
            }
        }
    }
    println!("extents with len-sum mismatch: {bad_sum}; non-final short chunks: {short_mid}");

    // The extent covering `target` (by the chunkstore's rule: first match).
    println!("\nextents covering partition byte {target}:");
    for (ei, ext) in stream.extents.iter().enumerate() {
        let start = ext.start_sector * EXTENT_LBA_BYTES as u64;
        let end = start + ext.sector_count * EXTENT_LBA_BYTES as u64;
        if target >= start && target < end {
            let mut chunks = by_extent.get(&(ei as u32)).cloned().unwrap_or_default();
            chunks.sort_by_key(|c| c.chunk_index);
            println!(
                "  extent {ei}: bytes [{start}..{end}) len {} — {} chunks",
                end - start,
                chunks.len()
            );
            let in_extent = target - start;
            let ord = in_extent / CHUNK_SIZE as u64;
            for c in &chunks {
                let lo = c.chunk_index as i64 - 2;
                if (c.chunk_index as i64) >= lo.max(ord as i64 - 2)
                    && (c.chunk_index as u64) <= ord + 2
                {
                    // recompute the hash of this chunk's decompressed data
                    let data = reader.read_chunk(c).unwrap();
                    let got = blake3::hash(&data).to_hex().to_string();
                    let expect = records
                        .iter()
                        .find(|r| {
                            r.extent_index == c.extent_index && r.chunk_index == c.chunk_index
                        })
                        .map(|r| r.blake3.clone())
                        .unwrap_or_else(|| "<none>".into());
                    println!(
                        "    chunk idx {} len {} file_off {} — hash {} manifest {} {}",
                        c.chunk_index,
                        c.uncompressed_len,
                        c.file_offset,
                        &got[..16],
                        &expect[..16.min(expect.len())],
                        if got == expect { "OK" } else { "MISMATCH" }
                    );
                }
            }
        }
    }

    // Full side-by-side for the extent covering `target` (and the tail of
    // the previous extent): stream chunk table vs manifest records, in the
    // order each table stores them.
    println!("\nstream chunks (extent 0 last 3, extent 1 all):");
    for (i, c) in stream.chunks.iter().enumerate() {
        let last_of_0 = c.extent_index == 0
            && stream.chunks.iter().filter(|x| x.extent_index == 0).count() - 3
                <= stream.chunks[..i].iter().filter(|x| x.extent_index == 0).count();
        if last_of_0 || c.extent_index == 1 {
            println!(
                "  [{i}] ext {} chunk {} len {} file_off {}",
                c.extent_index, c.chunk_index, c.uncompressed_len, c.file_offset
            );
        }
    }
    println!("\nmanifest records (extent 0 last 3, extent 1 all):");
    let n0 = records.iter().filter(|r| r.extent_index == 0).count();
    let mut seen0 = 0;
    for (i, r) in records.iter().enumerate() {
        if r.extent_index == 0 {
            seen0 += 1;
            if seen0 + 3 <= n0 {
                continue;
            }
        } else if r.extent_index != 1 {
            continue;
        }
        println!(
            "  [{i}] ext {} chunk {} len {} blake3 {}",
            r.extent_index,
            r.chunk_index,
            r.uncompressed_len,
            &r.blake3[..16]
        );
    }
}
