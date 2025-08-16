use crate::args::{CreateCommand, InstallCommand, Manifest};
use crate::create;
use crate::process::CommandExt;
use crate::storage::{self, BlockDevice, MountStack};
use crate::tool::Tool;
use anyhow::anyhow;
use console::style;
use dialoguer::{Confirm, Select, theme::ColorfulTheme};
use log::{info, warn};
use nix::mount::MsFlags;
use std::fs;
use std::path::{Path, PathBuf};

const MANIFEST_PATH: &str = "/usr/share/alma/manifest.json";

pub fn install(command: InstallCommand) -> anyhow::Result<()> {
    // 1. Check if we are on a valid ALMA system by finding the manifest
    info!("Looking for ALMA installation manifest...");
    let manifest_file = Path::new(MANIFEST_PATH);
    if !manifest_file.exists() {
        return Err(anyhow!(
            "Manifest file not found at {}. This command can only be run from a system created by 'alma create'.",
            MANIFEST_PATH
        ));
    }
    let manifest: Manifest = serde_json::from_str(&fs::read_to_string(manifest_file)?)?;
    info!("Found manifest for a '{}' system.", manifest.system_variant);

    // 2. Determine target device/partitions
    // This logic is now mutually exclusive thanks to clap's `conflicts_with_all`
    let (target_path, root_partition, boot_partition) = if let Some(path) = command.target_device {
        (Some(path), None, None)
    } else if command.root_partition.is_some() {
        // When using partitions, the "device" path for wiping is None.
        (None, command.root_partition, command.boot_partition)
    } else {
        let current_disk_name = get_current_root_disk();
        let selected_path = select_target_device(
            command.allow_non_removable,
            command.noconfirm,
            current_disk_name,
        )?;
        (Some(selected_path), None, None)
    };

    // 3. Confirm with user
    if !command.noconfirm {
        let target_str = target_path.as_ref().map_or_else(
            || root_partition.as_ref().unwrap().display().to_string(),
            |p| p.display().to_string(),
        );
        let warning = if target_path.is_some() {
            "WIPE ALL DATA"
        } else {
            "REFORMAT THE PARTITION"
        };

        let confirmed = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "{} This will {} on {}. Continue?",
                style("WARNING:").red().bold(),
                warning,
                target_str
            ))
            .default(false)
            .interact()?;
        if !confirmed {
            return Err(anyhow!("User aborted installation."));
        }
    }

    // 4. Reconstruct the CreateCommand
    let reconstructed_cmd = CreateCommand {
        path: target_path,
        root_partition,
        boot_partition,
        system: manifest.system_variant,
        filesystem: manifest.filesystem,
        encrypted_root: manifest.encrypted_root,
        aur_helper: manifest.aur_helper.parse()?,
        noconfirm: true,
        allow_non_removable: command.allow_non_removable,
        presets: manifest
            .sources
            .iter()
            .filter(|s| s.r#type == "preset")
            .map(|s| s.baked_path.to_str().unwrap().parse().unwrap())
            .collect(),
        extra_packages: vec![],
        aur_packages: vec![],
        boot_size: None,
        interactive: false,
        image: None,
        overwrite: true,
        dryrun: false,
        pacman_conf: None,
    };

    // 5. Run the create command logic
    info!("Starting installation...");
    let device_path_for_migration = reconstructed_cmd.path.clone();
    create::create(reconstructed_cmd)?;

    // 6. Copy user data and configs
    let copy_data = if command.noconfirm {
        true
    } else {
        Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(
                "Do you want to copy user data and network configs to the new installation?",
            )
            .default(true)
            .interact()?
    };

    if copy_data {
        // Need to figure out the actual device path from the partition path
        // This is a complex problem. For now, we'll assume the user installed to a full disk if they want to copy.
        // A more robust solution would require parsing lsblk or udev.
        // For now, we make this part conditional on having a full device path.
        if let Some(device_path) = &device_path_for_migration {
            migrate_system_data(device_path)?;
        } else {
            warn!(
                "Cannot automatically migrate data when installing to pre-existing partitions. Please copy /home and /etc/NetworkManager/system-connections manually."
            );
        }
    }

    info!("System installation successful!");
    Ok(())
}

fn migrate_system_data(target_device_path: &Path) -> anyhow::Result<()> {
    info!("Migrating user data and system configurations...");
    let rsync = Tool::find("rsync", false)?;
    let arch_chroot = Tool::find("arch-chroot", false)?;

    let storage_device = storage::StorageDevice::from_path(target_device_path, true, false)?;
    let root_partition = storage_device.get_partition(crate::constants::ROOT_PARTITION_INDEX)?;
    let mount_point = tempfile::tempdir()?;
    let mut mount_stack = MountStack::new(false);
    // Since this is a simple mount, we pass empty flags and no specific data.
    mount_stack.mount_single(
        root_partition.path(),
        mount_point.path(),
        None, // Let the kernel auto-detect the fs type (ext4 or btrfs)
        MsFlags::empty(),
        None,
    )?;

    // --- Copy /home ---
    info!("Copying /home directory...");
    let home_dest = mount_point.path().join("home/");
    rsync
        .execute()
        .arg("-a")
        .arg("--info=progress2")
        .arg("/home/")
        .arg(&home_dest)
        .run(false)?;
    for entry in fs::read_dir("/home")?.filter_map(Result::ok) {
        if entry.path().is_dir() {
            let user = entry.file_name();
            info!("Correcting ownership for user: {}", user.to_string_lossy());
            arch_chroot
                .execute()
                .arg(mount_point.path())
                .args([
                    "chown",
                    "-R",
                    &format!("{0}:{0}", user.to_string_lossy()),
                    &format!("/home/{}", user.to_string_lossy()),
                ])
                .run(false)?;
        }
    }

    // --- Copy NetworkManager connections ---
    info!("Copying NetworkManager connections...");
    let nm_source = Path::new("/etc/NetworkManager/system-connections");
    if nm_source.exists() {
        let nm_dest = mount_point
            .path()
            .join("etc/NetworkManager/system-connections");
        if !nm_dest.exists() {
            fs::create_dir_all(&nm_dest)?;
        }
        fs_extra::dir::copy(
            nm_source,
            mount_point.path().join("etc/NetworkManager/"),
            &fs_extra::dir::CopyOptions::new().overwrite(true),
        )?;

        // Secure the copied connection files
        info!("Securing copied network configurations...");
        arch_chroot
            .execute()
            .arg(mount_point.path())
            .args([
                "chown",
                "-R",
                "root:root",
                "/etc/NetworkManager/system-connections",
            ])
            .run(false)?;
        arch_chroot
            .execute()
            .arg(mount_point.path())
            .args(["chmod", "600", "/etc/NetworkManager/system-connections/*"])
            .run(false)?;
    }

    info!("Data migration complete.");
    Ok(())
}

fn select_target_device(
    allow_non_removable: bool,
    noconfirm: bool,
    current_device_name: Option<String>,
) -> anyhow::Result<PathBuf> {
    if noconfirm {
        return Err(anyhow!(
            "In non-interactive mode, the target device must be specified."
        ));
    }
    let mut devices = storage::get_storage_devices(allow_non_removable)?;
    // Filter out the device we are currently running from
    if let Some(name) = current_device_name {
        devices.retain(|d| d.name != name.trim());
    }

    if devices.is_empty() {
        return Err(anyhow!("No other storage devices found to install to."));
    }

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a target device to install to")
        .default(0)
        .items(&devices)
        .interact()?;
    Ok(PathBuf::from("/dev").join(&devices[selection].name))
}

/// Finds the parent disk device (e.g., "sda", "nvme0n1") for the currently running root filesystem.
fn get_current_root_disk() -> Option<String> {
    info!("Determining the current root disk to exclude it from the target list...");

    // 1. Read /proc/mounts to find the device mounted at /
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    let root_mount_line = mounts.lines().find(|line| {
        let mut parts = line.split_whitespace();
        let _device = parts.next();
        let mount_point = parts.next();
        mount_point == Some("/")
    })?;

    let root_partition_path = root_mount_line.split_whitespace().next()?;
    info!("Root filesystem is on partition: {root_partition_path}");

    // 2. Use lsblk to find the parent disk (PKNAME) of the root partition.
    // This is the most reliable way to handle names like /dev/sda1, /dev/nvme0n1p1, etc.
    let output = std::process::Command::new("lsblk")
        .arg("-no")
        .arg("PKNAME")
        .arg(root_partition_path)
        .output()
        .ok()?;

    if !output.status.success() {
        warn!("lsblk failed, cannot determine current root disk.");
        return None;
    }

    let disk_name = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if disk_name.is_empty() {
        warn!("lsblk returned empty name, cannot determine current root disk.");
        return None;
    }

    info!("Current root disk identified as: {disk_name}");
    Some(disk_name)
}
