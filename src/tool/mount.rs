use crate::args::FilesystemTypeArg;
use crate::storage::{Filesystem, MountStack};
use anyhow::Context;
use log::{debug, info};
use std::fs;
use std::path::Path;

/// Mounts filesystems to the target directory.
/// This function is aware of filesystem types and will set up
/// Btrfs subvolumes correctly.
pub fn mount<'a>(
    mount_path: &Path,
    boot_filesystem: &'a Option<Filesystem>,
    root_filesystem: &'a Filesystem,
    dryrun: bool,
) -> anyhow::Result<MountStack<'a>> {
    let mut mount_stack = MountStack::new(dryrun);
    let root_device_path = root_filesystem.block().path();
    info!("Mounting filesystems to {}", mount_path.display());

    if root_filesystem.fs_type() == FilesystemTypeArg::Btrfs {
        // --- BTRFS Subvolume Mounting Logic ---
        debug!("Btrfs filesystem detected, mounting subvolumes.");

        // 1. Mount root subvolume '@'
        let opts = "compress=zstd:3,noatime,subvol=@";
        mount_stack.mount_single(root_device_path, mount_path, Some(opts))?;

        // 2. Create directories for other subvolumes inside the root mount
        if !dryrun {
            fs::create_dir_all(mount_path.join("home"))?;
            fs::create_dir_all(mount_path.join("var/log"))?;
            fs::create_dir_all(mount_path.join("var/cache/pacman/pkg"))?;
        }

        // 3. Mount other subvolumes
        let home_opts = "compress=zstd:3,noatime,subvol=@home";
        mount_stack.mount_single(root_device_path, &mount_path.join("home"), Some(home_opts))?;

        let log_opts = "compress=zstd:3,noatime,subvol=@log";
        mount_stack.mount_single(
            root_device_path,
            &mount_path.join("var/log"),
            Some(log_opts),
        )?;

        let pkg_opts = "compress=zstd:3,noatime,subvol=@pkg";
        mount_stack.mount_single(
            root_device_path,
            &mount_path.join("var/cache/pacman/pkg"),
            Some(pkg_opts),
        )?;
    } else {
        // --- Standard EXT4 Mounting Logic ---
        debug!("EXT4 filesystem detected, performing standard mount.");
        mount_stack.mount_single(root_device_path, mount_path, Some("noatime"))?;
    }

    // Mount boot partition to /boot (common to both fs types)
    if let Some(boot_sys) = boot_filesystem {
        let boot_point = mount_path.join("boot");
        if !dryrun && !boot_point.exists() {
            fs::create_dir(&boot_point).context("Error creating the boot directory")?;
        }
        mount_stack.mount_single(boot_sys.block().path(), &boot_point, None)?;
    }

    Ok(mount_stack)
}
