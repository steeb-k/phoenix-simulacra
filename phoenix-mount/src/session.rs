//! A live mount: a materialized VHD image attached read-only. Dropping the
//! session detaches the disk and deletes the temp image.

use std::path::{Path, PathBuf};

use phoenix_core::container::PhnxReader;
use phoenix_core::error::Result;

use crate::attach::AttachedDisk;
use crate::image::{self, MaterializedImage};

pub struct MountSession {
    _attached: AttachedDisk,
    image_path: PathBuf,
    pub backup_path: PathBuf,
    pub disk_size: u64,
}

impl MountSession {
    /// Mount `backup` read-only: materialize it into a fixed-VHD image under
    /// `scratch_dir` (a sparse temp file) and attach it. The contained volumes
    /// then appear with drive letters in Explorer.
    pub fn mount(backup: &Path, scratch_dir: &Path) -> Result<Self> {
        let reader = PhnxReader::open(backup)?;
        std::fs::create_dir_all(scratch_dir)?;
        let stem = backup
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "backup".into());
        let image_path =
            scratch_dir.join(format!("{stem}-{}.vhd", reader.header.backup_id.simple()));

        let MaterializedImage {
            path, disk_size, ..
        } = image::materialize(reader, &image_path)?;

        let attached =
            AttachedDisk::attach_readonly(path.to_str().ok_or_else(|| {
                phoenix_core::error::PhoenixError::Other("non-UTF-8 path".into())
            })?)?;

        Ok(Self {
            _attached: attached,
            image_path: path,
            backup_path: backup.to_path_buf(),
            disk_size,
        })
    }
}

impl Drop for MountSession {
    fn drop(&mut self) {
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
