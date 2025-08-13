use crate::args::FilesystemTypeArg;
use crate::storage::{Filesystem, MountStack};
use anyhow::Context;
use log::info;
use nix::mount::MsFlags;
use std::fs;
use std::path::Path;

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
        // For Btrfs, we pass subvol options via the `data` parameter.
        let common_flags = MsFlags::MS_NOATIME;

        let root_data = "compress=zstd:3,subvol=@";
        mount_stack.mount_single(
            root_device_path,
            mount_path,
            Some("btrfs"),
            common_flags,
            Some(root_data),
        )?;

        if !dryrun {
            fs::create_dir_all(mount_path.join("home"))?;
            fs::create_dir_all(mount_path.join("var/log"))?;
            fs::create_dir_all(mount_path.join("var/cache/pacman/pkg"))?;
        }

        let home_data = "compress=zstd:3,subvol=@home";
        mount_stack.mount_single(
            root_device_path,
            &mount_path.join("home"),
            Some("btrfs"),
            common_flags,
            Some(home_data),
        )?;

        let log_data = "compress=zstd:3,subvol=@log";
        mount_stack.mount_single(
            root_device_path,
            &mount_path.join("var/log"),
            Some("btrfs"),
            common_flags,
            Some(log_data),
        )?;

        let pkg_data = "compress=zstd:3,subvol=@pkg";
        mount_stack.mount_single(
            root_device_path,
            &mount_path.join("var/cache/pacman/pkg"),
            Some("btrfs"),
            common_flags,
            Some(pkg_data),
        )?;
    } else {
        // --- Standard EXT4 Mounting Logic ---
        // For ext4, we pass `noatime` as a flag, and `data` is None.
        mount_stack.mount(
            root_filesystem,
            mount_path.to_path_buf(),
            MsFlags::MS_NOATIME,
        )?;
    }

    // Mount boot partition to /boot
    if let Some(boot_sys) = boot_filesystem {
        let boot_point = mount_path.join("boot");
        if !dryrun && !boot_point.exists() {
            fs::create_dir(&boot_point).context("Error creating the boot directory")?;
        }
        // Boot partition has no special flags.
        mount_stack.mount(boot_sys, boot_point, MsFlags::empty())?;
    }

    Ok(mount_stack)
}
