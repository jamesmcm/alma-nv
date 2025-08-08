use crate::args::{CreateCommand, InstallCommand, Manifest};
use crate::create;
use crate::process::CommandExt;
use crate::storage;
use crate::tool::Tool;
use anyhow::{Context, anyhow};
use console::style;
use dialoguer::{Confirm, Select, theme::ColorfulTheme};
use log::{info, warn};
use std::fs;
use std::path::{Path, PathBuf};

const MANIFEST_PATH: &str = "/usr/share/alma/manifest.json";

fn check_internet() -> bool {
    ureq::get("http://archlinux.org").call().is_ok()
}

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
    info!(
        "Found manifest for a '{}' system.",
        manifest.system_variant.to_string()
    );

    // 2. Determine target device/partitions
    // This logic is now mutually exclusive thanks to clap's `conflicts_with_all`
    let (target_path, root_partition, boot_partition) = if let Some(path) = command.target_device {
        (Some(path), None, None)
    } else {
        // When using partitions, the "device" path for wiping is None.
        (None, command.root_partition, command.boot_partition)
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
    };

    // 5. Run the create command logic
    info!("Starting installation...");
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
        if let Some(device_path) = &reconstructed_cmd.path {
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

    let mut storage_device = storage::StorageDevice::from_path(target_device_path, true, false)?;
    let root_partition = storage_device.get_partition(crate::constants::ROOT_PARTITION_INDEX)?;
    let mount_point = tempfile::tempdir()?;
    let mut mount_stack = MountStack::new(false);
    mount_stack.mount_single(root_partition.path(), mount_point.path(), None)?;

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

fn copy_home_directory(target_device_path: &Path) -> anyhow::Result<()> {
    info!("Copying /home directory to the new system...");
    let rsync_tool = Tool::find("rsync", false)?;

    // We need to mount the new system's root partition to copy files into it.
    let mut storage_device = storage::StorageDevice::from_path(target_device_path, true, false)?;
    let root_partition = storage_device.get_partition(crate::constants::ROOT_PARTITION_INDEX)?;

    let mount_point = tempfile::tempdir()?;
    let mut mount_cmd = Tool::find("mount", false)?.execute();
    mount_cmd
        .arg(root_partition.path())
        .arg(mount_point.path())
        .run(false)?;

    let home_dest = mount_point.path().join("home/");
    info!("rsync -a /home/ {}", home_dest.display());
    rsync_tool
        .execute()
        .arg("-a") // archive mode, preserves everything
        .arg("--info=progress2")
        .arg("/home/")
        .arg(home_dest)
        .run(false)
        .context("Failed to copy /home directory with rsync.")?;

    // Chown the copied files inside the new system
    info!("Correcting file ownership in new /home...");
    let arch_chroot_tool = Tool::find("arch-chroot", false)?;
    for entry in fs::read_dir("/home")? {
        let entry = entry?;
        let user = entry.file_name();
        if entry.path().is_dir() {
            arch_chroot_tool
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

    Tool::find("umount", false)?
        .execute()
        .arg(mount_point.path())
        .run(false)?;
    info!("Home directory copied successfully.");
    Ok(())
}
