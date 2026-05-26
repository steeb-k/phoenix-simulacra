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

    /// Translate an above-boundary source cluster to its destination,
    /// or `None` if no entry covers it. Clusters at or below
    /// `safe_max_cluster` are not mapped.
    pub fn translate_cluster(&self, src_cluster: u64) -> Option<u64> {
        if src_cluster <= self.safe_max_cluster {
            return None;
        }
        for entry in &self.entries {
            if src_cluster >= entry.src_cluster_start && src_cluster < entry.src_end_cluster() {
                let offset = src_cluster - entry.src_cluster_start;
                return Some(entry.dst_cluster_start + offset);
            }
        }
        None
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
