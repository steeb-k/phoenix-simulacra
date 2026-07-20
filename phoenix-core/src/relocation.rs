//! Cluster-level relocation map shared between the restore planner and
//! the capture-side disk writer.
//!
//! The *builder* lives in [`phoenix_restore::relocation`] — it has all
//! the manifest knowledge needed to compute free runs and pack
//! above-boundary clusters. The *type* lives here so the disk-write
//! path in `phoenix-capture/src/raw.rs` can consume it without taking a
//! dependency on `phoenix-restore` (which already depends on
//! `phoenix-capture`, so the reverse would be a cycle).
//!
//! See `phoenix-restore/src/relocation.rs` for the construction
//! algorithm and end-to-end design notes.

/// One mapping rule: every source cluster in
/// `[src_cluster_start, src_cluster_start + cluster_count)` should be
/// written at the corresponding offset in
/// `[dst_cluster_start, dst_cluster_start + cluster_count)` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelocationEntry {
    pub src_cluster_start: u64,
    pub cluster_count: u64,
    pub dst_cluster_start: u64,
}

impl RelocationEntry {
    pub fn src_end_cluster(&self) -> u64 {
        self.src_cluster_start.saturating_add(self.cluster_count)
    }
    pub fn dst_end_cluster(&self) -> u64 {
        self.dst_cluster_start.saturating_add(self.cluster_count)
    }
}

/// A complete plan for shrinking one partition. Entries describe the
/// post-boundary cluster ranges; clusters at or below
/// `safe_max_cluster` keep their original positions and are not in any
/// entry.
///
/// Entries are sorted by `src_cluster_start` so callers can binary
/// search them when translating writes.
#[derive(Debug, Clone)]
pub struct RelocationMap {
    /// Source's logical sector size in bytes (e.g. 512 or 4096).
    pub sector_size: u64,
    /// Source's cluster size in bytes (e.g. 4096 for default-formatted
    /// NTFS).
    pub cluster_size: u64,
    /// Inclusive last cluster index that fits in the target partition.
    pub safe_max_cluster: u64,
    /// New total cluster count for the target partition.
    pub new_total_clusters: u64,
    /// Sorted by `src_cluster_start`.
    pub entries: Vec<RelocationEntry>,
}

/// One write of a contiguous range of bytes from a chunk to a target
/// byte offset. `source_offset` is the byte index within the chunk's
/// uncompressed buffer where this segment starts; `len` is its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteSegment {
    pub dst_byte_offset: u64,
    pub source_offset: usize,
    pub len: usize,
}

impl RelocationMap {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry covering `src_cluster`, found by binary search.
    ///
    /// Relies on `entries` being sorted by `src_cluster_start` (a documented
    /// invariant of this type). A linear scan here is what made metadata
    /// rewriting quadratic: it runs once per relocated cluster, so a map with
    /// thousands of entries turned a large shrink into billions of compares.
    fn entry_covering(&self, src_cluster: u64) -> Option<&RelocationEntry> {
        let after = self
            .entries
            .partition_point(|e| e.src_cluster_start <= src_cluster);
        after
            .checked_sub(1)
            .and_then(|i| self.entries.get(i))
            .filter(|e| src_cluster < e.src_end_cluster())
    }

    /// Translate an above-boundary source cluster to its destination,
    /// or `None` if no entry covers it. Clusters at or below
    /// `safe_max_cluster` are not mapped.
    pub fn translate_cluster(&self, src_cluster: u64) -> Option<u64> {
        if src_cluster <= self.safe_max_cluster {
            return None;
        }
        self.entry_covering(src_cluster)
            .map(|e| e.dst_cluster_start + (src_cluster - e.src_cluster_start))
    }

    /// Translate `src_cluster` and report how many clusters from there map
    /// contiguously, capped at `max_len`.
    ///
    /// This is the bulk form of [`translate_cluster`](Self::translate_cluster).
    /// Callers walking a run list want whole spans, not clusters: asking one
    /// cluster at a time costs a lookup per cluster, while a run that lies
    /// inside a single entry — the overwhelmingly common case — is answered
    /// here by one binary search. Returns at least 1 for any `max_len >= 1`.
    pub fn translate_span(&self, src_cluster: u64, max_len: u64) -> (u64, u64) {
        if max_len == 0 {
            return (src_cluster, 0);
        }
        if src_cluster <= self.safe_max_cluster {
            // Identity, up to the boundary.
            let span = (self.safe_max_cluster + 1 - src_cluster).min(max_len);
            return (src_cluster, span);
        }
        match self.entry_covering(src_cluster) {
            Some(e) => {
                let offset = src_cluster - e.src_cluster_start;
                let span = (e.cluster_count - offset).min(max_len);
                (e.dst_cluster_start + offset, span)
            }
            None => {
                // Above the boundary but unmapped: identity, up to wherever
                // the next entry begins so the caller re-checks there.
                let next = self
                    .entries
                    .get(
                        self.entries
                            .partition_point(|e| e.src_cluster_start <= src_cluster),
                    )
                    .map(|e| e.src_cluster_start - src_cluster)
                    .unwrap_or(max_len);
                (src_cluster, next.clamp(1, max_len))
            }
        }
    }

    /// Translate a write of `len` bytes starting at source byte
    /// `src_byte_offset` into one or more target writes. See the module
    /// docs (and this method's tests in `phoenix-restore`) for examples.
    pub fn translate_write(&self, src_byte_offset: u64, len: usize) -> Vec<WriteSegment> {
        let mut segments = Vec::new();
        if len == 0 {
            return segments;
        }
        let cs = self.cluster_size;
        let safe_byte_boundary = self.safe_max_cluster.saturating_add(1).saturating_mul(cs);
        let src_end = src_byte_offset.saturating_add(len as u64);
        let mut cursor = src_byte_offset;
        let mut written = 0usize;

        while cursor < src_end {
            let remaining_chunk = (src_end - cursor) as usize;
            let (segment_len, dst_byte) = if cursor < safe_byte_boundary {
                let seg_end = safe_byte_boundary.min(src_end);
                let seg_len = (seg_end - cursor) as usize;
                (seg_len.min(remaining_chunk), cursor)
            } else {
                let entry = self.entries.iter().find(|e| {
                    let e_start = e.src_cluster_start.saturating_mul(cs);
                    let e_end = e.src_end_cluster().saturating_mul(cs);
                    cursor >= e_start && cursor < e_end
                });
                match entry {
                    Some(e) => {
                        let e_start_byte = e.src_cluster_start.saturating_mul(cs);
                        let e_end_byte = e.src_end_cluster().saturating_mul(cs);
                        let cluster_offset = cursor - e_start_byte;
                        let dst = e.dst_cluster_start.saturating_mul(cs) + cluster_offset;
                        let seg_end = e_end_byte.min(src_end);
                        let seg_len = (seg_end - cursor) as usize;
                        (seg_len.min(remaining_chunk), dst)
                    }
                    None => {
                        // Map should cover all above-boundary writes;
                        // surface as an identity write so verify catches
                        // the corruption rather than silently mis-writing.
                        tracing::error!(
                            cursor,
                            len = remaining_chunk,
                            "relocation map missing entry for above-boundary write; falling back to identity"
                        );
                        (remaining_chunk, cursor)
                    }
                }
            };
            if segment_len == 0 {
                tracing::error!(
                    cursor,
                    src_end,
                    "translate_write produced zero-length segment; aborting translation"
                );
                break;
            }
            segments.push(WriteSegment {
                dst_byte_offset: dst_byte,
                source_offset: written,
                len: segment_len,
            });
            cursor = cursor.saturating_add(segment_len as u64);
            written += segment_len;
        }
        segments
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_with(entries: Vec<RelocationEntry>, safe_max: u64) -> RelocationMap {
        RelocationMap {
            sector_size: 512,
            cluster_size: 4096,
            safe_max_cluster: safe_max,
            new_total_clusters: safe_max + 1,
            entries,
        }
    }

    /// `translate_span` is the bulk form of `translate_cluster`; if they ever
    /// disagree, a relocated run list points somewhere the data isn't.
    #[test]
    fn span_translation_agrees_with_per_cluster_translation() {
        let map = map_with(
            vec![
                RelocationEntry {
                    src_cluster_start: 100,
                    cluster_count: 10,
                    dst_cluster_start: 5,
                },
                RelocationEntry {
                    src_cluster_start: 110,
                    cluster_count: 4,
                    dst_cluster_start: 40,
                },
                // Deliberate gap: 114..120 is above the boundary but unmapped.
                RelocationEntry {
                    src_cluster_start: 120,
                    cluster_count: 8,
                    dst_cluster_start: 60,
                },
            ],
            99,
        );
        for start in 0..140u64 {
            let mut cursor = start;
            while cursor < 140 {
                let (dst, span) = map.translate_span(cursor, 140 - cursor);
                assert!(span >= 1, "no progress at {cursor}");
                for i in 0..span {
                    let expect = map.translate_cluster(cursor + i).unwrap_or(cursor + i);
                    assert_eq!(
                        expect,
                        dst + i,
                        "cluster {} disagrees inside span at {cursor}",
                        cursor + i
                    );
                }
                cursor += span;
            }
        }
    }

    #[test]
    fn a_span_stops_at_the_shrink_boundary() {
        let map = map_with(
            vec![RelocationEntry {
                src_cluster_start: 100,
                cluster_count: 10,
                dst_cluster_start: 5,
            }],
            99,
        );
        // Below-boundary clusters are identity, and the span must not run
        // past cluster 99 into mapped territory.
        let (dst, span) = map.translate_span(90, 50);
        assert_eq!(dst, 90);
        assert_eq!(span, 10);
    }

    #[test]
    fn entry_lookup_finds_the_covering_entry_only() {
        let map = map_with(
            vec![
                RelocationEntry {
                    src_cluster_start: 200,
                    cluster_count: 5,
                    dst_cluster_start: 0,
                },
                RelocationEntry {
                    src_cluster_start: 300,
                    cluster_count: 5,
                    dst_cluster_start: 10,
                },
            ],
            99,
        );
        assert_eq!(map.translate_cluster(200), Some(0));
        assert_eq!(map.translate_cluster(204), Some(4));
        assert_eq!(map.translate_cluster(205), None); // just past the entry
        assert_eq!(map.translate_cluster(299), None); // in the gap
        assert_eq!(map.translate_cluster(300), Some(10));
        assert_eq!(map.translate_cluster(50), None); // below the boundary
    }
}
