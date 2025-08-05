use super::Tool;
use super::mount;
use crate::args;
use crate::constants::{BOOT_PARTITION_INDEX, ROOT_PARTITION_INDEX};
use crate::process::CommandExt;
use crate::storage;
use crate::storage::{BlockDevice, Filesystem, FilesystemType, LoopDevice};
use crate::storage::{EncryptedDevice, is_encrypted_device};
use anyhow::Context;
use log::info;

use tempfile::tempdir;

/// Use arch-chroot to chroot to the given device
/// Also handles encrypted root partitions (detected by checking for the LUKS magic header)
pub fn chroot(command: args::ChrootCommand) -> anyhow::Result<()> {
    let arch_chroot = Tool::find("arch-chroot", false)?;
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

    // TODO: Here we assume fixed indexes for boot and root partitions - may not be the case for custom partitions
    let boot_partition = storage_device.get_partition(BOOT_PARTITION_INDEX)?;
    let boot_filesystem = Filesystem::from_partition(&boot_partition, FilesystemType::Vfat);

    let root_partition_base = storage_device.get_partition(ROOT_PARTITION_INDEX)?;
    let encrypted_root = if is_encrypted_device(&root_partition_base)? {
        cryptsetup = Some(Tool::find("cryptsetup", false)?);
        Some(EncryptedDevice::open(
            cryptsetup.as_ref().expect("cryptsetup not found"),
            &root_partition_base,
            "alma_root".into(),
        )?)
    } else {
        None
    };

    let root_partition = if let Some(e) = encrypted_root.as_ref() {
        e as &dyn BlockDevice
    } else {
        &root_partition_base as &dyn BlockDevice
    };
    let root_filesystem = Filesystem::from_partition(root_partition, FilesystemType::Ext4);

    let boot_sys = Some(boot_filesystem);
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
