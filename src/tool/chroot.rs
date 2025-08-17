use super::Tool;
use super::mount;
use crate::args;
use crate::process::CommandExt;
use crate::storage;
use crate::storage::filesystem::FilesystemType;
use crate::storage::{BlockDevice, Filesystem, LoopDevice, partition::Partition};
use crate::storage::{EncryptedDevice, is_encrypted_device};
use anyhow::{Context, anyhow};
use log::info;
use std::path::PathBuf;

use tempfile::tempdir;

/// Use arch-chroot to chroot to the given device
/// Also handles encrypted root partitions (detected by checking for the LUKS magic header)
pub fn chroot(command: args::ChrootCommand) -> anyhow::Result<()> {
    let arch_chroot = Tool::find("arch-chroot", false)?;
    let blkid = Tool::find("blkid", false)?;
    let sfdisk = Tool::find("sfdisk", false)?;
    let cryptsetup;

    let loop_device: Option<LoopDevice>;
    let storage_device = match storage::StorageDevice::from_path(
        &command.block_device,
        command.allow_non_removable,
        false,
    ) {
        Ok(b) => b,
        Err(_) => {
            loop_device = Some(LoopDevice::create(&command.block_device, false)?);
            storage::StorageDevice::from_path(
                loop_device.as_ref().expect("loop device not found").path(),
                command.allow_non_removable,
                false,
            )?
        }
    };
    let mount_point = tempdir().context("Error creating a temporary directory")?;

    // --- Automatic Partition and Filesystem Detection ---
    info!(
        "Discovering partitions on {}",
        storage_device.path().display()
    );
    let partition_list_raw = sfdisk
        .execute()
        .args(["-l", "-o", "Device"])
        .arg(storage_device.path())
        .run_text_output(false)?;

    let partitions: Vec<PathBuf> = partition_list_raw
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();

    if partitions.is_empty() {
        return Err(anyhow!(
            "No partitions found on {}",
            storage_device.path().display()
        ));
    }

    let mut boot_partition_opt: Option<Partition> = None;
    let mut root_partition_base_opt: Option<Partition> = None;
    let mut root_fs_type_opt: Option<FilesystemType> = None;

    for part_path in &partitions {
        let partition = Partition::new::<storage::StorageDevice>(part_path.clone());

        if is_encrypted_device(&partition)? {
            if root_partition_base_opt.is_some() {
                return Err(anyhow!(
                    "Found multiple potential root partitions (LUKS encrypted). Ambiguous layout."
                ));
            }
            root_partition_base_opt = Some(partition);
            continue;
        }

        let fs_type_str = blkid
            .execute()
            .args(["-s", "TYPE", "-o", "value"])
            .arg(part_path)
            .run_text_output(false)
            .unwrap_or_default();

        match fs_type_str.trim() {
            "vfat" => {
                if boot_partition_opt.is_some() {
                    return Err(anyhow!(
                        "Found multiple potential boot partitions (vfat). Ambiguous layout."
                    ));
                }
                boot_partition_opt = Some(partition);
            }
            "ext4" => {
                if root_partition_base_opt.is_some() {
                    return Err(anyhow!(
                        "Found multiple potential root partitions (ext4 and previous). Ambiguous layout."
                    ));
                }
                root_partition_base_opt = Some(partition);
                root_fs_type_opt = Some(FilesystemType::Ext4);
            }
            "btrfs" => {
                if root_partition_base_opt.is_some() {
                    return Err(anyhow!(
                        "Found multiple potential root partitions (btrfs and previous). Ambiguous layout."
                    ));
                }
                root_partition_base_opt = Some(partition);
                root_fs_type_opt = Some(FilesystemType::Btrfs);
            }
            _ => {} // Ignore swap, etc.
        }
    }

    let root_partition_base = root_partition_base_opt.ok_or_else(|| {
        anyhow!("Could not find a suitable root partition (ext4, btrfs, or LUKS).")
    })?;

    let encrypted_root = if is_encrypted_device(&root_partition_base)? {
        cryptsetup = Some(Tool::find("cryptsetup", false)?);
        Some(EncryptedDevice::open(
            cryptsetup.as_ref().unwrap(),
            &root_partition_base,
            "alma_root".into(),
        )?)
    } else {
        None
    };

    let root_partition: &dyn BlockDevice = encrypted_root
        .as_ref()
        .map_or(&root_partition_base, |e| e as &dyn BlockDevice);

    let root_fs_type = if let Some(fs_type) = root_fs_type_opt {
        fs_type
    } else {
        // We have an encrypted device, so we must check the type on the opened container
        let fs_type_str = blkid
            .execute()
            .args(["-s", "TYPE", "-o", "value"])
            .arg(root_partition.path())
            .run_text_output(false)?;
        match fs_type_str.trim() {
            "ext4" => FilesystemType::Ext4,
            "btrfs" => FilesystemType::Btrfs,
            other => {
                return Err(anyhow!(
                    "Unsupported filesystem type '{}' on encrypted container.",
                    other
                ));
            }
        }
    };
    let root_filesystem = Filesystem::from_partition(root_partition, root_fs_type);

    let boot_sys = boot_partition_opt
        .as_ref()
        .map(|p| Filesystem::from_partition(p, FilesystemType::Vfat));
    let mount_stack = mount(mount_point.path(), &boot_sys, &root_filesystem, false)?;

    arch_chroot
        .execute()
        .arg(mount_point.path())
        .args(&command.command)
        .run(false)
        .with_context(|| {
            format!(
                "Error running command in chroot: {}",
                command.command.join(" "),
            )
        })?;

    info!("Unmounting filesystems");
    mount_stack.umount()?;

    Ok(())
}
