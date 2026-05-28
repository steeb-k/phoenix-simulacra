use std::mem;
use std::ptr;
use std::time::Duration;

use phoenix_core::disk::{open_disk_readonly, open_volume_readonly};
use phoenix_core::error::{PhoenixError, Result};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::Storage::FileSystem::ReadFile;

pub struct PartitionReader {
    handle: HANDLE,
    offset: u64,
    length: u64,
    /// True once `FSCTL_LOCK_VOLUME` has succeeded on `handle`. Drop is
    /// responsible for issuing the matching `FSCTL_UNLOCK_VOLUME` before
    /// closing the handle so a panic / cancel / mid-loop error can't leave
    /// the volume locked behind us.
    locked: bool,
}

impl PartitionReader {
    pub fn open_disk_partition(disk_path: &str, offset_bytes: u64, length_bytes: u64) -> Result<Self> {
        let handle = open_disk_readonly(disk_path)?;
        Ok(Self {
            handle,
            offset: offset_bytes,
            length: length_bytes,
            locked: false,
        })
    }

    pub fn open_volume(volume_path: &str) -> Result<Self> {
        let handle = open_volume_readonly(volume_path)?;
        let mut size = 0u64;
        #[repr(C)]
        struct LengthInfo {
            length: i64,
        }
        let mut info = LengthInfo { length: 0 };
        let mut returned = 0u32;
        let ok = unsafe {
            windows_sys::Win32::System::IO::DeviceIoControl(
                handle,
                windows_sys::Win32::System::Ioctl::IOCTL_DISK_GET_LENGTH_INFO,
                ptr::null(),
                0,
                &mut info as *mut _ as *mut _,
                mem::size_of::<LengthInfo>() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok != 0 {
            size = info.length as u64;
        }
        Ok(Self {
            handle,
            offset: 0,
            length: size,
            locked: false,
        })
    }

    pub fn from_path(path: &str, offset: u64, length: u64) -> Result<Self> {
        if path.contains("PhysicalDrive") {
            Self::open_disk_partition(path, offset, length)
        } else {
            let _ = (offset, length);
            Self::open_volume(path)
        }
    }

    pub fn read_at(&mut self, position: u64, buf: &mut [u8]) -> Result<usize> {
        if position >= self.length {
            return Ok(0);
        }
        let abs_pos = self.offset + position;
        unsafe {
            let mut dist = 0i64;
            let low = (abs_pos & 0xFFFF_FFFF) as u32;
            let high = (abs_pos >> 32) as u32;
            if windows_sys::Win32::Storage::FileSystem::SetFilePointerEx(
                self.handle,
                ((high as i64) << 32) | low as i64,
                &mut dist,
                0,
            ) == 0
            {
                return Err(PhoenixError::Disk("seek failed".into()));
            }
        }
        let to_read = buf.len().min((self.length - position) as usize);
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr() as *mut _,
                to_read as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(PhoenixError::Disk("read failed".into()));
        }
        Ok(read as usize)
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    /// Acquire `FSCTL_LOCK_VOLUME` on this reader's handle so the volume
    /// can't be opened, written, or browsed by anything else for the rest
    /// of the reader's lifetime. The IOCTL only succeeds when no other
    /// handles have files open on the volume; Windows internally flushes
    /// any dirty FS-cache pages before returning success, so the on-disk
    /// state is coherent at the moment we acquire the lock.
    ///
    /// Retries with exponential backoff (5 attempts, ~250 ms → 4 s,
    /// ~7.75 s total) before giving up. Transient holders like an
    /// antivirus scan or a just-closed Explorer window will usually clear
    /// in that window. On permanent failure a [`PhoenixError::VolumeLockFailed`]
    /// is returned with the failing drive label and the last Win32 error,
    /// so the caller can surface an actionable message to the user instead
    /// of silently degrading to a smeared live read.
    ///
    /// `label` is the user-facing drive identifier (e.g. `"\\\\.\\E:"` or
    /// `"E:"`) used in log messages and the error variant; it has no
    /// semantic effect on the lock itself.
    pub fn lock_volume(&mut self, label: &str) -> Result<()> {
        const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;
        const ATTEMPTS: u32 = 5;

        let mut backoff_ms: u64 = 250;
        let mut last_err: u32 = 0;
        for attempt in 1..=ATTEMPTS {
            let mut returned = 0u32;
            let ok = unsafe {
                windows_sys::Win32::System::IO::DeviceIoControl(
                    self.handle,
                    FSCTL_LOCK_VOLUME,
                    ptr::null(),
                    0,
                    ptr::null_mut(),
                    0,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            if ok != 0 {
                self.locked = true;
                tracing::info!(volume = label, attempt, "FSCTL_LOCK_VOLUME acquired");
                return Ok(());
            }
            last_err = unsafe { GetLastError() };
            tracing::warn!(
                volume = label,
                attempt,
                err = last_err,
                "FSCTL_LOCK_VOLUME failed; another process has files open on the volume"
            );
            if attempt < ATTEMPTS {
                std::thread::sleep(Duration::from_millis(backoff_ms));
                // Cap backoff at ~4 s so the total wait stays bounded even
                // if the loop bound is bumped later.
                backoff_ms = backoff_ms.saturating_mul(2).min(4_000);
            }
        }
        Err(PhoenixError::VolumeLockFailed {
            drive: label.to_string(),
            last_error: last_err,
        })
    }

    /// Whether this reader currently holds an `FSCTL_LOCK_VOLUME`.
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Ask the filesystem driver for its cluster allocation bitmap via
    /// `FSCTL_GET_VOLUME_BITMAP`. Each bit of the returned vector covers one
    /// cluster (LSB-first), so `bit (cluster_index)` set means that cluster is
    /// allocated. Returns `None` when the IOCTL isn't applicable to this
    /// handle (e.g. the reader was opened against a raw `\\.\PhysicalDriveN`
    /// rather than a mounted volume) or fails for any reason — callers are
    /// expected to fall back to a more conservative capture path in that
    /// case rather than treat the absence of a bitmap as an error.
    ///
    /// Loops over `ERROR_MORE_DATA` so the full bitmap is assembled even when
    /// the FS driver can't fit it in a single response. The `total_clusters`
    /// argument bounds the output and protects against a misreporting driver.
    pub fn try_volume_bitmap(&mut self, total_clusters: u64) -> Option<Vec<u8>> {
        if total_clusters == 0 {
            return None;
        }

        // FSCTL_GET_VOLUME_BITMAP lives in the System::Ioctl module; the
        // associated input/output structs aren't all re-exported by
        // `windows-sys`, so we declare them here (they're stable Win32 ABI).
        #[repr(C)]
        struct StartingLcnInputBuffer {
            starting_lcn: i64,
        }
        #[repr(C)]
        struct VolumeBitmapHeader {
            starting_lcn: i64,
            // BitmapSize is in clusters/bits, NOT bytes (per MSDN).
            bitmap_size: i64,
        }

        const FSCTL_GET_VOLUME_BITMAP: u32 = 0x0009_006F;
        // Hardcoded Win32 error codes so we don't need to pull additional
        // Foundation features into Cargo.toml just for these constants.
        const ERROR_MORE_DATA: u32 = 234;
        const ERROR_INVALID_PARAMETER: u32 = 87;
        // NTFS reserves the trailing sector(s) for the backup boot sector,
        // so the FS-reported cluster count is typically `total_clusters - 1`
        // (where `total_clusters` is what we computed from the boot sector).
        // Use a small but generous margin to absorb that and any other
        // single-cluster trailing reservation a driver might impose.
        const FS_TRAILING_RESERVE: u64 = 8;

        let header_size = mem::size_of::<VolumeBitmapHeader>();
        let bitmap_bytes = ((total_clusters + 7) / 8) as usize;
        let mut full_bitmap = vec![0u8; bitmap_bytes];
        let mut next_lcn: i64 = 0;
        // Termination is driven by the IOCTL itself: we keep going while it
        // returns ERROR_MORE_DATA and stop on the first non-zero return
        // (success). The iteration counter is purely a runaway-prevention
        // belt to guard against a misbehaving driver that returns
        // ERROR_MORE_DATA without making forward progress.
        let mut iterations = 0u32;

        loop {
            iterations += 1;
            if iterations > 4096 {
                tracing::warn!(
                    next_lcn,
                    "FSCTL_GET_VOLUME_BITMAP: bailing after 4096 iterations to avoid a runaway loop"
                );
                return None;
            }

            let input = StartingLcnInputBuffer {
                starting_lcn: next_lcn,
            };

            // Some NTFS drivers fail FSCTL_GET_VOLUME_BITMAP with very large
            // METHOD_NEITHER output buffers — the IOCTL succeeds with smaller
            // buffers and surfaces the rest through ERROR_MORE_DATA. Cap each
            // response at 4 MiB; that's still ~32 Mi clusters per round-trip
            // (~128 GiB of cluster space at 4 KiB clusters), so a multi-TB
            // volume is covered in a few dozen calls without surprising the
            // driver.
            const CHUNK_MAX: usize = 4 * 1024 * 1024;
            let remaining_clusters = total_clusters.saturating_sub(next_lcn as u64);
            let needed_bytes = ((remaining_clusters + 7) / 8) as usize + header_size + 16;
            let buf_size = needed_bytes.min(CHUNK_MAX).max(header_size + 4096);
            let mut buf = vec![0u8; buf_size];

            let mut returned = 0u32;
            let ok = unsafe {
                windows_sys::Win32::System::IO::DeviceIoControl(
                    self.handle,
                    FSCTL_GET_VOLUME_BITMAP,
                    &input as *const _ as *const _,
                    mem::size_of::<StartingLcnInputBuffer>() as u32,
                    buf.as_mut_ptr() as *mut _,
                    buf_size as u32,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
            let err = unsafe { GetLastError() };

            if ok == 0 && err != ERROR_MORE_DATA {
                // Reactive end-of-volume safety net. Some NTFS drivers
                // (notably USB-attached volumes / shadow copies on client
                // SKUs) signal ERROR_MORE_DATA on the chunk that lands at
                // the FS's last cluster and then reject the follow-up
                // query at that LCN with ERROR_INVALID_PARAMETER (87). If
                // that's what just happened — we were already up against
                // the boot-sector cluster boundary — keep the bitmap we
                // already assembled rather than discarding it.
                if err == ERROR_INVALID_PARAMETER
                    && (next_lcn as u64) + FS_TRAILING_RESERVE >= total_clusters
                {
                    tracing::debug!(
                        starting_lcn = next_lcn,
                        total_clusters = total_clusters,
                        "FSCTL_GET_VOLUME_BITMAP rejected end-of-volume probe; using assembled bitmap"
                    );
                    break;
                }
                // WARN (not DEBUG) because the fallback path silently turns
                // a used-blocks backup into a full raw clone of the
                // partition, which on a multi-TB drive looks like a hang to
                // the user. Surfacing the Win32 error code is the only way
                // to triage a driver that refuses the IOCTL.
                tracing::warn!(
                    err = err,
                    starting_lcn = next_lcn,
                    buf_size = buf_size,
                    total_clusters = total_clusters,
                    "FSCTL_GET_VOLUME_BITMAP failed; caller will fall back to full-partition capture"
                );
                return None;
            }
            if (returned as usize) < header_size {
                tracing::warn!(
                    returned = returned,
                    header_size = header_size,
                    "FSCTL_GET_VOLUME_BITMAP: response too short for header"
                );
                return None;
            }

            let header = unsafe { &*(buf.as_ptr() as *const VolumeBitmapHeader) };
            // The IOCTL rounds the requested StartingLcn DOWN to a multiple
            // of 8, so the returned bitmap is always byte-aligned and can be
            // copied with byte-level slicing.
            let starting_lcn = header.starting_lcn as u64;
            // CAREFUL: `BitmapSize` from VOLUME_BITMAP_BUFFER is the
            // *entire volume's* cluster count, NOT the count covered by
            // this single response. Per MSDN:
            //
            //   "BitmapSize: The size of the bitmap, in clusters. [...]
            //   The Buffer member contains as much of the bitmap as can
            //   fit in the output buffer."
            //
            // So on a 952 GiB volume, every response says
            // `bitmap_size = 249_764_864` regardless of how few bytes
            // the driver actually wrote into Buffer. Using that value
            // to advance `next_lcn` would mark the loop "done" after
            // one iteration even when only the first 4 MiB chunk got
            // populated — which is exactly the 128 GiB capture cap we
            // hit in production. The right value for "did we make
            // forward progress in this response?" is the *payload
            // length*, since each byte covers 8 clusters.
            let bitmap_size_clusters = header.bitmap_size as u64;
            let payload = &buf[header_size..returned as usize];
            let payload_clusters = (payload.len() as u64).saturating_mul(8);

            let dst_start = (starting_lcn / 8) as usize;
            if dst_start < full_bitmap.len() {
                let dst_end = (dst_start + payload.len()).min(full_bitmap.len());
                if dst_end > dst_start {
                    full_bitmap[dst_start..dst_end]
                        .copy_from_slice(&payload[..dst_end - dst_start]);
                }
            }

            tracing::debug!(
                iteration = iterations,
                starting_lcn,
                payload_bytes = payload.len(),
                payload_clusters,
                driver_bitmap_size_clusters = bitmap_size_clusters,
                ok = ok != 0,
                err = err,
                "FSCTL_GET_VOLUME_BITMAP chunk"
            );

            // Advance by *clusters covered in this response*, not by the
            // driver's total-volume size hint. See the comment on
            // `bitmap_size_clusters` above for the gory detail.
            let next = starting_lcn + payload_clusters;
            if next <= next_lcn as u64 {
                // No forward progress.
                //
                // If `ok != 0` (success), treat as "I'm done": there's
                // legitimately no more bitmap to send. This is the
                // small-volume single-call path *and* the
                // driver-says-done-on-final-chunk path.
                //
                // If `ok == 0 && err == ERROR_MORE_DATA` we got
                // promised "more data" but the driver didn't actually
                // advance — that's a buggy driver and we have to bail
                // rather than spin forever.
                if ok != 0 {
                    break;
                }
                tracing::warn!(
                    next,
                    next_lcn,
                    payload_clusters,
                    bitmap_size_clusters,
                    "FSCTL_GET_VOLUME_BITMAP: ERROR_MORE_DATA without forward progress; aborting"
                );
                return None;
            }
            // Per MSDN, the driver's view of "end of bitmap" is at
            // `starting_lcn + bitmap_size_clusters` (the BitmapSize field
            // is the count of clusters from StartingLcn to the end of the
            // volume, not the chunk size). Use the *minimum* of that and
            // the caller's total_clusters as the termination target — they
            // should be equal modulo NTFS's backup-boot-sector reservation,
            // and taking the min keeps us conservative if either side is
            // off by a cluster.
            let driver_end = starting_lcn.saturating_add(bitmap_size_clusters);
            let coverage_target = total_clusters.min(driver_end);
            if next + FS_TRAILING_RESERVE >= coverage_target {
                break;
            }
            // Driver said success but didn't actually reach the end of
            // the volume. Log it and keep iterating — the loop will
            // either get more data on the next call, or terminate via
            // the ERROR_INVALID_PARAMETER end-of-volume safety net.
            if ok != 0 {
                tracing::debug!(
                    starting_lcn,
                    payload_clusters,
                    next,
                    coverage_target,
                    "FSCTL_GET_VOLUME_BITMAP returned success but only covered part of the \
                     volume; continuing to paginate (driver-reported completion is unreliable \
                     on this handle, typically a VSS shadow or locked NTFS volume)"
                );
            }
            next_lcn = next as i64;
        }

        // Quick sanity log at INFO level so an operator scanning a log
        // file can immediately tell whether the bitmap was fully read
        // or truncated. Pre-fix runs would terminate after iteration 1
        // and capture only the first 32 Mi clusters (~128 GiB) of a
        // larger volume; with the fix we expect iterations to scale
        // with total_clusters / (CHUNK_MAX_clusters).
        let last_covered_lcn = next_lcn as u64;
        tracing::info!(
            iterations,
            total_clusters,
            last_covered_lcn,
            full_bitmap_bytes = full_bitmap.len(),
            "FSCTL_GET_VOLUME_BITMAP loop done"
        );

        Some(full_bitmap)
    }
}

impl Drop for PartitionReader {
    fn drop(&mut self) {
        if self.locked {
            // Best-effort unlock. Closing the handle would release the lock
            // implicitly anyway, but issuing the matching IOCTL first makes
            // the release point explicit in any kernel tracing / ETW logs
            // and avoids a small window where the volume looks stuck-locked
            // to anyone querying right as we close.
            const FSCTL_UNLOCK_VOLUME: u32 = 0x0009_001C;
            let mut returned = 0u32;
            let _ = unsafe {
                windows_sys::Win32::System::IO::DeviceIoControl(
                    self.handle,
                    FSCTL_UNLOCK_VOLUME,
                    ptr::null(),
                    0,
                    ptr::null_mut(),
                    0,
                    &mut returned,
                    ptr::null_mut(),
                )
            };
        }
        unsafe { CloseHandle(self.handle) };
    }
}

pub fn open_physical_disk(path: &str) -> Result<HANDLE> {
    open_disk_readonly(path)
}
