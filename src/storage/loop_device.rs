use crate::{process::CommandExt, tool::Tool};
use anyhow::Context;
use log::{debug, info, warn};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub struct LoopDevice {
    path: PathBuf,
    losetup: Tool,
    dryrun: bool,
}

impl LoopDevice {
    pub fn create(file: &Path, dryrun: bool) -> anyhow::Result<Self> {
        let losetup = Tool::find("losetup", dryrun)?;
        let output = losetup
            .execute()
            .args(["--find", "-P", "--show"])
            .arg(file)
            .run_text_output(dryrun)
            .context("Error creating the image")?;

        let path = if dryrun {
            PathBuf::from("/dev/loop1337")
        } else {
            PathBuf::from(output.trim())
        };
        info!("Mounted {} to {}", file.display(), path.display());

        Ok(Self {
            path,
            losetup,
            dryrun,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LoopDevice {
    fn drop(&mut self) {
        info!("Detaching loop device {}", self.path.display());
        if self.dryrun {
            return;
        }
        unmount_loop_mounts(&self.path);
        if let Err(error) = self
            .losetup
            .execute()
            .arg("-d")
            .arg(&self.path)
            .run(self.dryrun)
        {
            warn!(
                "Failed to detach loop device {}; something still holds its partitions open (e.g. a desktop automounter) and the image file stays pinned: {error:#}",
                self.path.display()
            );
        }
    }
}

/// Desktop environments auto-mount freshly exposed loop partitions (typically
/// under /run/media). Those extra mounts keep the loop device busy and defeat
/// `losetup -d`, silently pinning the image file. Best-effort unmount of any
/// mount backed by this loop device or its partitions before detaching.
fn unmount_loop_mounts(loop_path: &Path) {
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return;
    };
    let loop_path = loop_path.to_string_lossy().to_string();
    let partition_prefix = format!("{loop_path}p");
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(device), Some(mount_point)) = (fields.next(), fields.next()) else {
            continue;
        };
        if device == loop_path || device.starts_with(&partition_prefix) {
            let unmount_result = Command::new("umount").arg(mount_point).run(false);
            match unmount_result {
                Ok(()) => info!("Unmounted automounted loop mount {mount_point}"),
                Err(error) => debug!("Could not unmount {mount_point}: {error:#}"),
            }
        }
    }
}
