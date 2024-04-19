use std::{fs, path::Path, thread, time::Duration};

use anyhow::Context;
use log::{debug, info};
use tempfile::TempDir;

use std::io::Write;

use crate::{
    constants, initcpio,
    process::CommandExt,
    storage::{partition::Partition, BlockDevice, EncryptedDevice, StorageDevice},
    tool::Tool,
};

pub struct DiskPartitions<'a> {
    pub boot_partition: Partition<'a>,
    pub root_partition_base: Partition<'a>,
}

pub fn repartition_disk<'a>(
    storage_device: &'a StorageDevice,
    boot_size_mb: u32,
    sgdisk: &Tool,
    dryrun: bool,
) -> anyhow::Result<DiskPartitions<'a>> {
    let disk_path = storage_device.path();

    info!("Partitioning the block device");
    debug!("{:?}", disk_path);

    sgdisk
        .execute()
        .args([
            "-Z",
            "-o",
            &format!("--new=1::+{}M", boot_size_mb),
            "--new=2::+1M",
            "--largest-new=3",
            "--typecode=1:EF00",
            "--typecode=2:EF02",
        ])
        .arg(disk_path)
        .run(dryrun)
        .context("Partitioning error")?;

    thread::sleep(Duration::from_millis(1000));

    info!("Formatting filesystems");
    let boot_partition = storage_device.get_partition(constants::BOOT_PARTITION_INDEX)?;
    let root_partition_base = storage_device.get_partition(constants::ROOT_PARTITION_INDEX)?;

    Ok(DiskPartitions {
        boot_partition,
        root_partition_base,
    })
}

pub fn setup_bootloader(
    storage_device: &StorageDevice,
    mount_point: &TempDir,
    arch_chroot: &Tool,
    encrypted_root: Option<&EncryptedDevice>,
    root_partition_base: &Partition,
    blkid: Option<&Tool>,
    dryrun: bool,
) -> anyhow::Result<()> {
    info!("Starting bootloader initialisation tasks");
    // If boot partition was generated or given, then it is already mounted at /boot in the MountStack by this stage

    let disk_path = storage_device.path();
    info!("Generating initramfs");
    let plymouth_exists = Path::new(&mount_point.path().join("usr/bin/plymouth")).exists();
    if !dryrun {
        fs::write(
            mount_point.path().join("etc/mkinitcpio.conf"),
            initcpio::Initcpio::new(encrypted_root.is_some(), plymouth_exists).to_config()?,
        )
        .context("Failed to write to mkinitcpio.conf")?;
    }
    arch_chroot
        .execute()
        .arg(mount_point.path())
        .args(["mkinitcpio", "-P"])
        .run(dryrun)
        .context("Failed to run mkinitcpio - do you have the base and linux packages installed?")?;

    if encrypted_root.is_some() {
        debug!("Setting up GRUB for an encrypted root partition");

        let uuid = blkid
            .expect("No tool for blkid")
            .execute()
            .arg(root_partition_base.path())
            .args(["-o", "value", "-s", "UUID"])
            .run_text_output(dryrun)
            .context("Failed to run blkid")?;
        let trimmed = uuid.trim();
        debug!("Root partition UUID: {}", trimmed);

        if !dryrun {
            let mut grub_file = fs::OpenOptions::new()
                .append(true)
                .open(mount_point.path().join("etc/default/grub"))
                .context("Failed to create /etc/default/grub")?;

            // TODO: Handle multiple encrypted partitions with osprober?
            write!(
                &mut grub_file,
                "GRUB_CMDLINE_LINUX=\"cryptdevice=UUID={}:luks_root\"",
                trimmed
            )
            .context("Failed to write to /etc/default/grub")?;
        }
    }

    // TODO: add grub os-prober?
    // TODO: Allow choice of bootloader - systemd-boot + refind?
    // TODO: Add systemd volatile root option
    info!("Installing the Bootloader");
    arch_chroot
        .execute()
        .arg(mount_point.path())
        .args(["bash", "-c"])
        .arg(format!("grub-install --target=i386-pc --boot-directory /boot {} && grub-install --target=x86_64-efi --efi-directory /boot --boot-directory /boot --removable &&  grub-mkconfig -o /boot/grub/grub.cfg", disk_path.display()))
        .run(dryrun).context("Failed to install grub")?;

    let bootloader = mount_point.path().join("boot/EFI/BOOT/BOOTX64.efi");

    if !dryrun {
        fs::rename(
            &bootloader,
            mount_point.path().join("boot/EFI/BOOT/grubx64.efi"),
        )
        .context("Cannot move out grub")?;
        fs::copy(
            mount_point.path().join("usr/share/shim-signed/mmx64.efi"),
            mount_point.path().join("boot/EFI/BOOT/mmx64.efi"),
        )
        .context("Failed copying mmx64")?;
        fs::copy(
            mount_point.path().join("usr/share/shim-signed/shimx64.efi"),
            bootloader,
        )
        .context("Failed copying shim")?;

        debug!(
            "GRUB configuration: {}",
            fs::read_to_string(mount_point.path().join("boot/grub/grub.cfg"))
                .unwrap_or_else(|e| e.to_string())
        );
    }
    Ok(())
}
