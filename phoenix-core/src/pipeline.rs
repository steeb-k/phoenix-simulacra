//! Parallel chunk pipeline: overlaps device I/O with the CPU work (BLAKE3 +
//! zstd) that used to run inline in one serial loop per chunk.
//!
//! Two halves, mirroring the two data directions:
//!
//! * **Encode** ([`EncodePipeline`]) — used by
//!   `PartitionStreamWriter::write_chunk` during capture. The capture loop
//!   keeps reading from the source device on its own thread and hands each
//!   plaintext chunk to a small worker pool that hashes + compresses in
//!   parallel; a dedicated writer thread commits the compressed chunks to the
//!   `.phnx` file **in submission order**, so the on-disk layout (chunk
//!   offsets, `ChunkIndex` order, `ChunkRecord` order) is byte-identical to
//!   what the old serial loop produced.
//!
//! * **Decode** ([`process_chunks_parallel`]) — used by restore and by the
//!   full-verify tier. A reader thread streams compressed chunks out of the
//!   `.phnx` file in file order, workers decompress (and optionally
//!   BLAKE3-verify) in parallel, and the caller's closure consumes the
//!   plaintext chunks **in order** on the calling thread — which is where the
//!   raw-disk writes have to stay (device handles are not shared across
//!   threads).
//!
//! Ordering is load-bearing in both directions: capture must reproduce the
//! serial file layout exactly, and restore wants monotonically increasing
//! target offsets so spinning/USB media never see a backwards seek storm.
//!
//! Memory is bounded by the channel capacities: every queue slot holds at
//! most one `CHUNK_SIZE` (4 MiB) buffer, and each direction keeps
//! ~4 buffers per worker in flight, so the worst case is on the order of
//! 128 MiB at the default 8-worker cap — a deliberate trade against the
//! multi-GB/s it buys on NVMe-class sources.

use std::collections::BTreeMap;
use std::fs::File;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::container::{compress_chunk, decompress_chunk, ChunkIndex};
use crate::error::{PhoenixError, Result};
use crate::hash;
use crate::manifest::ChunkRecord;
use crate::progress::ProgressHandle;

/// Number of hash/compress (or decompress) workers. Defaults to the machine's
/// core count capped at 8 — zstd level 3 runs ~400 MB/s per core, so 8 workers
/// already outrun any single source device we stream from. Override with
/// `PHOENIX_WORKERS` (clamped to 1..=64) for experiments.
pub fn worker_count() -> usize {
    if let Ok(v) = std::env::var("PHOENIX_WORKERS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            return n.clamp(1, 64);
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8)
}

/// `write_all` at an absolute file offset without touching the handle's seek
/// cursor — the writer thread shares the file with the main thread (which
/// seeks freely between partition streams), so positional I/O is mandatory.
fn write_all_at(file: &File, mut offset: u64, mut buf: &[u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_write(buf, offset)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "wrote 0 bytes",
            ));
        }
        offset += n as u64;
        buf = &buf[n..];
    }
    Ok(())
}

/// `read_exact` at an absolute file offset (positional, cursor-independent).
fn read_exact_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "read 0 bytes",
            ));
        }
        offset += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Encode side (capture)
// ---------------------------------------------------------------------------

struct EncodeJob {
    seq: u64,
    extent_index: u32,
    chunk_in_extent: u32,
    data: Vec<u8>,
}

struct EncodeDone {
    seq: u64,
    extent_index: u32,
    chunk_in_extent: u32,
    uncompressed_len: u32,
    hash_hex: String,
    compressed: Vec<u8>,
}

/// Everything the writer thread accumulated, handed back at
/// [`EncodePipeline::finish`]. Identical content, in identical order, to what
/// the old serial `write_chunk` loop built.
pub(crate) struct EncodeOutput {
    pub chunk_indices: Vec<ChunkIndex>,
    pub chunk_records: Vec<ChunkRecord>,
    /// File offset one past the last compressed byte (where the chunk index
    /// table goes).
    pub end_offset: u64,
}

/// Parallel hash+compress+write engine behind `PartitionStreamWriter`.
///
/// Error model: the first error from any worker or from the writer thread is
/// parked in a shared slot and the `failed` flag flips. Workers never exit
/// early and the writer keeps draining (discarding) results after a failure,
/// so no thread can deadlock on a full channel; the caller observes the error
/// on the next `submit` or at `finish`.
pub(crate) struct EncodePipeline {
    job_tx: Option<SyncSender<EncodeJob>>,
    workers: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<EncodeOutput>>,
    error: Arc<Mutex<Option<PhoenixError>>>,
    failed: Arc<AtomicBool>,
    next_seq: u64,
}

impl EncodePipeline {
    /// `file` must be a clone of the `.phnx` writer's handle; compressed
    /// chunks are committed starting at `data_start`.
    pub(crate) fn new(file: File, data_start: u64, progress: Option<ProgressHandle>) -> Self {
        let workers = worker_count();
        let (job_tx, job_rx) = sync_channel::<EncodeJob>(workers * 2);
        let (done_tx, done_rx) = sync_channel::<EncodeDone>(workers * 2);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let error: Arc<Mutex<Option<PhoenixError>>> = Arc::new(Mutex::new(None));
        let failed = Arc::new(AtomicBool::new(false));

        let mut worker_handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            let error = Arc::clone(&error);
            let failed = Arc::clone(&failed);
            worker_handles.push(std::thread::spawn(move || loop {
                // Hold the lock only for the recv; compression runs unlocked.
                let job = match job_rx.lock().expect("encode job lock").recv() {
                    Ok(j) => j,
                    Err(_) => return, // channel closed: no more chunks
                };
                if failed.load(Ordering::SeqCst) {
                    // Something already went wrong; keep draining jobs so the
                    // producer never blocks on a full queue, but skip the work.
                    continue;
                }
                let hash_hex = hash::hash_hex(&job.data);
                match compress_chunk(&job.data) {
                    Ok(compressed) => {
                        let done = EncodeDone {
                            seq: job.seq,
                            extent_index: job.extent_index,
                            chunk_in_extent: job.chunk_in_extent,
                            uncompressed_len: job.data.len() as u32,
                            hash_hex,
                            compressed,
                        };
                        if done_tx.send(done).is_err() {
                            return; // writer gone (only happens on teardown)
                        }
                    }
                    Err(e) => {
                        error.lock().expect("encode error lock").get_or_insert(e);
                        failed.store(true, Ordering::SeqCst);
                    }
                }
            }));
        }
        drop(done_tx); // writer's recv loop ends when the last worker exits

        let werror = Arc::clone(&error);
        let wfailed = Arc::clone(&failed);
        let writer = std::thread::spawn(move || {
            let mut pending: BTreeMap<u64, EncodeDone> = BTreeMap::new();
            let mut next = 0u64;
            let mut offset = data_start;
            let mut chunk_indices: Vec<ChunkIndex> = Vec::new();
            let mut chunk_records: Vec<ChunkRecord> = Vec::new();
            while let Ok(done) = done_rx.recv() {
                if wfailed.load(Ordering::SeqCst) {
                    continue; // drain without writing
                }
                pending.insert(done.seq, done);
                while let Some(d) = pending.remove(&next) {
                    if let Err(e) = write_all_at(&file, offset, &d.compressed) {
                        werror
                            .lock()
                            .expect("encode error lock")
                            .get_or_insert(PhoenixError::Io(e));
                        wfailed.store(true, Ordering::SeqCst);
                        break;
                    }
                    chunk_indices.push(ChunkIndex {
                        file_offset: offset,
                        compressed_len: d.compressed.len() as u32,
                        uncompressed_len: d.uncompressed_len,
                        extent_index: d.extent_index,
                        chunk_index: d.chunk_in_extent,
                    });
                    chunk_records.push(ChunkRecord {
                        chunk_index: chunk_records.len() as u32,
                        extent_index: d.extent_index,
                        uncompressed_len: d.uncompressed_len,
                        blake3: d.hash_hex,
                    });
                    offset += chunk_indices.last().unwrap().compressed_len as u64;
                    next += 1;
                    if let Some(ref p) = progress {
                        p.bump(chunk_records.last().unwrap().uncompressed_len as u64);
                    }
                }
            }
            if !pending.is_empty() && !wfailed.load(Ordering::SeqCst) {
                // Can only happen if a worker died without reporting — treat
                // as a hard internal error rather than writing a gappy stream.
                werror
                    .lock()
                    .expect("encode error lock")
                    .get_or_insert(PhoenixError::Other(
                        "encode pipeline lost a chunk (worker died without reporting)".into(),
                    ));
                wfailed.store(true, Ordering::SeqCst);
            }
            EncodeOutput {
                chunk_indices,
                chunk_records,
                end_offset: offset,
            }
        });

        Self {
            job_tx: Some(job_tx),
            workers: worker_handles,
            writer: Some(writer),
            error,
            failed,
            next_seq: 0,
        }
    }

    fn take_error(&self) -> PhoenixError {
        self.error
            .lock()
            .expect("encode error lock")
            .take()
            .unwrap_or_else(|| PhoenixError::Other("encode pipeline failed".into()))
    }

    /// Queue one plaintext chunk. Blocks (backpressure) when the pool is
    /// saturated, which is what keeps the source reader from racing ahead of
    /// the CPU/writer stages by more than the channel capacity.
    pub(crate) fn submit(
        &mut self,
        extent_index: u32,
        chunk_in_extent: u32,
        data: Vec<u8>,
    ) -> Result<()> {
        if self.failed.load(Ordering::SeqCst) {
            return Err(self.take_error());
        }
        let job = EncodeJob {
            seq: self.next_seq,
            extent_index,
            chunk_in_extent,
            data,
        };
        let tx = self
            .job_tx
            .as_ref()
            .expect("submit after finish is a bug in the caller");
        if tx.send(job).is_err() {
            return Err(self.take_error());
        }
        self.next_seq += 1;
        Ok(())
    }

    /// Close the intake, wait for every in-flight chunk to be committed, and
    /// hand back the accumulated tables. Returns the first pipeline error if
    /// any stage failed.
    pub(crate) fn finish(mut self) -> Result<EncodeOutput> {
        self.shutdown();
        let out = match self.writer.take() {
            Some(h) => h
                .join()
                .map_err(|_| PhoenixError::Other("encode writer thread panicked".into()))?,
            None => unreachable!("finish called twice"),
        };
        if self.failed.load(Ordering::SeqCst) {
            return Err(self.take_error());
        }
        if out.chunk_records.len() as u64 != self.next_seq {
            return Err(PhoenixError::Other(format!(
                "encode pipeline committed {} of {} submitted chunks",
                out.chunk_records.len(),
                self.next_seq
            )));
        }
        Ok(out)
    }

    fn shutdown(&mut self) {
        drop(self.job_tx.take());
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

impl Drop for EncodePipeline {
    fn drop(&mut self) {
        // Reached only when the stream is abandoned mid-capture (error or
        // cancel unwound the caller before `finish`): stop intake and join so
        // no detached thread keeps writing into the half-built file.
        self.shutdown();
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Decode side (restore + verify)
// ---------------------------------------------------------------------------

/// One compressed chunk to fetch, decompress, and (optionally) hash-verify.
#[derive(Clone)]
pub struct DecodeItem {
    pub file_offset: u64,
    pub compressed_len: u32,
    pub uncompressed_len: u32,
    /// `Some(expected)` makes the worker BLAKE3 the plaintext and fail with
    /// [`PhoenixError::HashMismatch`] on divergence; `None` skips hashing
    /// (restore without verify).
    pub expected_blake3: Option<String>,
    /// Only for error attribution in `HashMismatch`.
    pub partition_index: u32,
    pub chunk_index: u32,
}

/// Stream `items` (in the given order) out of `file`, decompress + verify on
/// a worker pool, and call `on_chunk(seq, plaintext)` **in item order** on the
/// calling thread. The first error from any stage — read, decompress, hash
/// mismatch, or the consumer itself — aborts the whole run.
///
/// The consumer runs on the caller's thread by design: restore's raw-disk
/// `PartitionWriter` handle stays where it was opened, and target writes are
/// issued in monotonically increasing offset order (kind to spinning media).
pub fn process_chunks_parallel<F>(file: &File, items: &[DecodeItem], mut on_chunk: F) -> Result<()>
where
    F: FnMut(usize, Vec<u8>) -> Result<()>,
{
    let workers = worker_count();
    // Tiny partitions (boot stubs, MSR) aren't worth six threads of setup.
    if workers == 1 || items.len() <= 2 {
        let mut compressed = Vec::new();
        for (seq, item) in items.iter().enumerate() {
            compressed.resize(item.compressed_len as usize, 0);
            read_exact_at(file, item.file_offset, &mut compressed)?;
            let data = decode_one(item, &compressed)?;
            on_chunk(seq, data)?;
        }
        return Ok(());
    }

    let file = file.try_clone()?;
    let items_for_reader: Vec<DecodeItem> = items.to_vec();
    let total = items.len();

    std::thread::scope(|scope| -> Result<()> {
        let (job_tx, job_rx) = sync_channel::<
            std::result::Result<(usize, DecodeItem, Vec<u8>), PhoenixError>,
        >(workers * 2);
        let (done_tx, done_rx) =
            sync_channel::<std::result::Result<(usize, Vec<u8>), PhoenixError>>(workers * 2);
        let job_rx = Arc::new(Mutex::new(job_rx));

        // Reader: sequential positional reads in file order (capture writes
        // chunks in exactly this order, so this is a forward-only scan).
        scope.spawn(move || {
            for (seq, item) in items_for_reader.into_iter().enumerate() {
                let mut buf = vec![0u8; item.compressed_len as usize];
                let msg = match read_exact_at(&file, item.file_offset, &mut buf) {
                    Ok(()) => Ok((seq, item, buf)),
                    Err(e) => Err(PhoenixError::Io(e)),
                };
                let failed = msg.is_err();
                if job_tx.send(msg).is_err() || failed {
                    return; // consumers gone, or nothing useful after an error
                }
            }
        });

        for _ in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let done_tx = done_tx.clone();
            scope.spawn(move || loop {
                let job = match job_rx.lock().expect("decode job lock").recv() {
                    Ok(j) => j,
                    Err(_) => return,
                };
                let msg = job.and_then(|(seq, item, compressed)| {
                    decode_one(&item, &compressed).map(|data| (seq, data))
                });
                if done_tx.send(msg).is_err() {
                    return;
                }
            });
        }
        drop(done_tx);

        // In-order consumption with a bounded reorder buffer (workers can
        // only run ahead of `next` by the channel capacities).
        let mut pending: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
        let mut next = 0usize;
        while next < total {
            let msg = done_rx.recv().map_err(|_| {
                PhoenixError::Other("decode pipeline ended before delivering every chunk".into())
            })?;
            let (seq, data) = msg?;
            pending.insert(seq, data);
            while let Some(data) = pending.remove(&next) {
                on_chunk(next, data)?;
                next += 1;
            }
        }
        Ok(())
        // On early return the channels drop here; blocked threads see a
        // disconnected channel and exit, then the scope joins them.
    })
}

fn decode_one(item: &DecodeItem, compressed: &[u8]) -> Result<Vec<u8>> {
    let data = decompress_chunk(compressed, item.uncompressed_len as usize)?;
    if let Some(ref expected) = item.expected_blake3 {
        if hash::hash_hex(&data) != *expected {
            return Err(PhoenixError::HashMismatch {
                partition_index: item.partition_index,
                chunk_index: item.chunk_index,
            });
        }
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn worker_count_is_sane() {
        let n = worker_count();
        assert!((1..=64).contains(&n));
    }

    /// Decode a hand-built file of compressed chunks through the parallel
    /// path and check order, content, and hash verification.
    #[test]
    fn decode_parallel_orders_and_verifies() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pipe-decode-{}.bin", uuid::Uuid::new_v4().simple()));
        let mut file = std::fs::File::create(&path).unwrap();

        // 40 chunks of varied size/content so workers genuinely interleave.
        let mut items = Vec::new();
        let mut plain = Vec::new();
        let mut offset = 0u64;
        for i in 0..40u32 {
            let len = 1000 + (i as usize * 37) % 5000;
            let data: Vec<u8> = (0..len)
                .map(|j| ((i as usize * 31 + j * 7) % 251) as u8)
                .collect();
            let compressed = compress_chunk(&data).unwrap();
            file.write_all(&compressed).unwrap();
            items.push(DecodeItem {
                file_offset: offset,
                compressed_len: compressed.len() as u32,
                uncompressed_len: len as u32,
                expected_blake3: Some(hash::hash_hex(&data)),
                partition_index: 0,
                chunk_index: i,
            });
            offset += compressed.len() as u64;
            plain.push(data);
        }
        file.flush().unwrap();
        drop(file);

        let file = std::fs::File::open(&path).unwrap();
        let mut seen = 0usize;
        process_chunks_parallel(&file, &items, |seq, data| {
            assert_eq!(seq, seen, "chunks must arrive in order");
            assert_eq!(data, plain[seq], "chunk {seq} content mismatch");
            seen += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, 40);

        // Poison one expected hash: the run must fail with HashMismatch.
        let mut bad = items.clone();
        bad[17].expected_blake3 = Some("00".repeat(32));
        bad[17].chunk_index = 99;
        let err = process_chunks_parallel(&file, &bad, |_, _| Ok(())).unwrap_err();
        match err {
            PhoenixError::HashMismatch { chunk_index, .. } => assert_eq!(chunk_index, 99),
            other => panic!("expected HashMismatch, got {other:?}"),
        }

        // Consumer error propagates and terminates cleanly.
        let err = process_chunks_parallel(&file, &items, |seq, _| {
            if seq == 5 {
                Err(PhoenixError::Cancelled)
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(matches!(err, PhoenixError::Cancelled));

        drop(file);
        let _ = std::fs::remove_file(&path);
    }
}
