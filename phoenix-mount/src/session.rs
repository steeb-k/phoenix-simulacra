//! A live mount: a materialized VHD image attached read-only. Dropping the
//! session detaches the disk and deletes the temp image.

use std::path::{Path, PathBuf};

use phoenix_core::container::PhnxReader;
use phoenix_core::error::Result;

use crate::attach::AttachedDisk;
use crate::image::{self, MaterializedImage};
use crate::letters::{self, MountedVolume};

pub struct MountSession {
    _attached: AttachedDisk,
    image_path: PathBuf,
    pub backup_path: PathBuf,
    pub disk_size: u64,
    pub volumes: Vec<MountedVolume>,
    /// Letters we assigned (selection mode); removed again on drop.
    assigned_letters: Vec<char>,
}

impl MountSession {
    /// Mount `backup` read-only with all volumes exposed (Windows assigns the
    /// drive letters).
    pub fn mount(backup: &Path, scratch_dir: &Path) -> Result<Self> {
        Self::mount_selected(backup, scratch_dir, None)
    }

    /// Mount `backup` read-only: materialize it into a fixed-VHD image under
    /// `scratch_dir` and attach it. With `selection: Some(indices)` only those
    /// partitions get drive letters; `None` exposes everything (mount-manager
    /// policy).
    pub fn mount_selected(
        backup: &Path,
        scratch_dir: &Path,
        selection: Option<&[u32]>,
    ) -> Result<Self> {
        let reader = PhnxReader::open(backup)?;
        std::fs::create_dir_all(scratch_dir)?;
        let stem = backup
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "backup".into());
        let image_path =
            scratch_dir.join(format!("{stem}-{}.vhd", reader.header.backup_id.simple()));

        let MaterializedImage {
            path,
            disk_size,
            spans,
        } = image::materialize(reader, &image_path)?;

        let attached = AttachedDisk::attach_readonly_opts(
            path.to_str()
                .ok_or_else(|| phoenix_core::error::PhoenixError::Other("non-UTF-8 path".into()))?,
            selection.is_none(),
        )?;

        let (volumes, assigned_letters) = match selection {
            Some(sel) => {
                let disk = attached.physical_drive_number()?;
                letters::expose_selected(disk, &spans, sel)?
            }
            None => (Vec::new(), Vec::new()),
        };

        Ok(Self {
            _attached: attached,
            image_path: path,
            backup_path: backup.to_path_buf(),
            disk_size,
            volumes,
            assigned_letters,
        })
    }
}

impl Drop for MountSession {
    fn drop(&mut self) {
        // Remove any letters we assigned while the volumes still exist.
        letters::remove_letters(&self.assigned_letters);
        // The temp image can't be deleted while the disk is still attached, and
        // `_attached` only detaches when its field drops (just after this body
        // returns). So delete on a short retry loop off-thread: the first few
        // attempts fail until the detach settles, then one succeeds.
        let path = self.image_path.clone();
        std::thread::spawn(move || {
            for _ in 0..15 {
                if std::fs::remove_file(&path).is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        });
    }
}
