use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use byte_unit::Byte;
use console::style;
use dialoguer::{Confirm, Select, theme::ColorfulTheme};
use log::{debug, info, warn};
use nix::mount::MsFlags;

use crate::args::{CreateCommand, Manifest, RootFilesystemType, Source, SystemVariant};
use crate::constants::{self, OMARCHY_REPO_URL};
use crate::constants::{DEFAULT_BOOT_MB, MAX_BOOT_MB, MIN_BOOT_MB, OMARCHY_MIN_TOTAL_GIB};
use crate::initcpio;
use crate::interactive::UserSettings;
use crate::presets::{PathWrapper, PresetsCollection, Script};
use crate::process::CommandExt;
use crate::storage::filesystem::FilesystemType;
use crate::storage::{
    self, BlockDevice, EncryptedDevice, Filesystem, LoopDevice, MountStack, StorageDevice,
    partition::Partition,
};
use crate::tool::mount;
use crate::tool::{Tool, Tools};
use tempfile::TempDir;

fn fix_fstab(fstab: &str) -> String {
    fstab
        .lines()
        .filter(|line| !line.contains("swap") && !line.starts_with('#'))
        .collect::<Vec<&str>>()
        .join("\n")
}

pub fn create(mut command: CreateCommand) -> anyhow::Result<()> {
    // --- Initial Command Validation & Adjustments ---
    validate_command(&command)?;
    adjust_command_for_system(&mut command)?;
    // We only prompt for user settings if we are NOT in non-interactive mode.
    let user_settings: Option<UserSettings> = if !command.noconfirm {
        Some(UserSettings::prompt()?)
    } else {
        info!(
            "--noconfirm specified, skipping interactive setup. System will be configured by presets."
        );
        None
    };

    let original_command_string = env::args().collect::<Vec<String>>().join(" ");
    let mut manifest_sources: Vec<Source> = Vec::new();

    // 1. Load presets. We do this first to validate environment variables.
    let presets_paths = command
        .presets
        .clone()
        .into_iter()
        .map(|p| p.into_path_wrapper(command.noconfirm))
        .collect::<anyhow::Result<Vec<PathWrapper>>>()?;

    for (i, _p_path) in presets_paths.iter().enumerate() {
        let origin_path = command.presets[i].to_string();
        let baked_path = PathBuf::from("/usr/share/alma/baked_sources").join(format!("preset_{i}"));
        manifest_sources.push(Source {
            r#type: "preset".to_string(),
            origin: origin_path,
            baked_path,
        });
    }

    let presets = PresetsCollection::load(
        &presets_paths
            .iter()
            .map(|x| x.to_path())
            .collect::<Vec<&Path>>(),
    )?;

    // 2. Prepare tools
    let tools = Tools::new(&command)?;

    // 3. Resolve device path and create image file if needed
    let (storage_device_path, _image_loop) = resolve_device_path_and_image(&command)?;
    let mut storage_device = StorageDevice::from_path(
        &storage_device_path,
        command.allow_non_removable,
        command.dryrun,
    )?;

    // Check total device/image size for Omarchy
    if command.system == SystemVariant::Omarchy {
        let min_total_bytes =
            byte_unit::Byte::from_u64_with_unit(OMARCHY_MIN_TOTAL_GIB, byte_unit::Unit::GiB)
                .unwrap()
                .as_u128();

        let total_size = if let Some(image_size) = command.image {
            image_size
        } else {
            storage_device.size()
        };

        if total_size.as_u128() < min_total_bytes {
            warn!(
                "The selected device/image size ({}) is less than the recommended minimum of {} for Omarchy.",
                total_size.get_appropriate_unit(byte_unit::UnitType::Both),
                byte_unit::Byte::from_u128(min_total_bytes)
                    .expect("Failed to convert min_total_bytes")
                    .get_appropriate_unit(byte_unit::UnitType::Both)
            );
            if !command.noconfirm {
                let confirmed = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("Do you want to continue with this size?")
                    .default(false)
                    .interact()?;
                if !confirmed {
                    return Err(anyhow!(
                        "User aborted operation due to insufficient device size for Omarchy."
                    ));
                }
            }
        }
    }

    // 4. Safety checks and partitioning
    confirm_and_wipe_device(&mut storage_device, &command)?;
    let (boot_partition, root_partition_base) =
        partition_and_format(&command, &tools, &storage_device)?;

    // 5. Open encrypted container if requested
    let encrypted_root = if command.encrypted_root {
        Some(EncryptedDevice::open(
            tools.cryptsetup.as_ref().unwrap(),
            &root_partition_base,
            "alma_root".into(),
        )?)
    } else {
        None
    };
    let root_block_device: &dyn BlockDevice = encrypted_root
        .as_ref()
        .map_or(&root_partition_base, |e| e as &dyn BlockDevice);
    let root_fs_type: FilesystemType = command.filesystem.into();

    if root_fs_type == FilesystemType::Btrfs {
        setup_btrfs_subvolumes(
            root_block_device,
            tools.mkbtrfs.as_ref().ok_or_else(|| {
                anyhow!("Please install the btrfs-progs package to create btrfs filesystems")
            })?,
            tools.btrfs.as_ref().ok_or_else(|| {
                anyhow!("Please install the btrfs-progs package to create btrfs filesystems")
            })?,
            command.dryrun,
        )?;
    } else {
        Filesystem::format(
            root_block_device,
            root_fs_type,
            tools.mkext4.as_ref().context("mkfs.ext4 tool missing")?,
        )?;
    }

    let boot_filesystem = boot_partition
        .as_ref()
        .map(|p| Filesystem::from_partition(p, FilesystemType::Vfat));
    let root_filesystem = Filesystem::from_partition(root_block_device, root_fs_type);

    // 6. Bootstrap system
    // The `bootstrap_system` function now implicitly uses the new smart `mount` tool
    let (mount_point, mount_stack) = bootstrap_system(
        &command,
        &tools,
        &boot_filesystem,
        &root_filesystem,
        &presets,
        user_settings.as_ref(),
    )?;

    // 7. Copy baked sources into the image
    bake_sources_into_image(&tools, mount_point.path(), &presets_paths, &command)?;

    if let Some(settings) = &user_settings {
        info!("Applying settings from interactive setup...");
        let setup_script = settings.generate_setup_script()?;
        run_script_in_chroot(
            &setup_script,
            &tools.arch_chroot,
            mount_point.path(),
            command.dryrun,
        )?;
    }

    // 8. Apply customizations (AUR, presets)
    apply_customizations(&command, &tools.arch_chroot, &presets, mount_point.path())?;

    // 9. Finalize installation (bootloader, services)
    finalize_installation(
        &command,
        &tools,
        &storage_device,
        &mount_point,
        encrypted_root.as_ref(),
        &root_partition_base,
    )?;

    // 10. Install Omarchy if requested
    if command.system == SystemVariant::Omarchy {
        // We need the username. In interactive mode, we have it.
        // In non-interactive, presets are expected to have created the user.
        // We will default to a common name if not in interactive mode, but this path is less robust.
        let username = user_settings.as_ref().map_or("user", |s| &s.username);
        install_omarchy(&tools, mount_point.path(), &command, username)?;
    }

    // 11. Generate manifest
    generate_manifest(
        &command,
        &mount_point,
        &original_command_string,
        &mut manifest_sources,
    )?;

    // 12. Interactive chroot and cleanup
    interactive_chroot_and_cleanup(
        &command,
        &tools.arch_chroot,
        mount_point.path(),
        mount_stack,
    )?;

    info!("Installation complete!");
    Ok(())
}

/// Creates a btrfs filesystem and the standard subvolume layout.
fn setup_btrfs_subvolumes(
    device: &dyn BlockDevice,
    mkbtrfs: &Tool,
    btrfs: &Tool,
    dryrun: bool,
) -> anyhow::Result<()> {
    info!("Creating Btrfs filesystem with subvolumes...");
    // 1. Format the partition
    mkbtrfs
        .execute()
        .arg("-f")
        .arg("-L")
        .arg("alma-root")
        .arg(device.path())
        .run(dryrun)?;

    // 2. Mount top-level to create subvolumes
    let temp_mount = tempfile::tempdir().context("Failed to create temp dir for btrfs setup")?;
    let mut temp_mount_stack = MountStack::new(dryrun);

    // We pass `noatime` as a flag and the `data` (options string) as None.
    temp_mount_stack.mount_single(
        device.path(),
        temp_mount.path(),
        Some("btrfs"), // Be explicit about the type
        MsFlags::MS_NOATIME,
        None,
    )?;

    // 3. Create subvolumes
    let subvolumes = ["@", "@home", "@log", "@pkg"];
    for vol in &subvolumes {
        let vol_path = temp_mount.path().join(vol);
        info!("Creating subvolume: {}", vol_path.display());
        btrfs
            .execute()
            .arg("subvolume")
            .arg("create")
            .arg(&vol_path)
            .run(dryrun)?;
    }

    // 4. Unmount, the MountStack's Drop will handle this automatically
    Ok(())
}

fn validate_command(command: &CreateCommand) -> anyhow::Result<()> {
    if matches!(command.system, SystemVariant::Omarchy) && command.noconfirm {
        return Err(anyhow!(
            "Non-interactive installation (--noconfirm) is not supported for Omarchy."
        ));
    }
    if command.encrypted_root && command.noconfirm {
        return Err(anyhow!(
            "Non-interactive encrypted root setup is not supported. The passphrase must be entered manually."
        ));
    }
    Ok(())
}

fn adjust_command_for_system(command: &mut CreateCommand) -> anyhow::Result<()> {
    if command.system == SystemVariant::Omarchy {
        let user_set_fs = env::args().any(|arg| arg.starts_with("--filesystem"));
        if user_set_fs && command.filesystem == RootFilesystemType::Ext4 {
            warn!("You have selected the ext4 filesystem for an Omarchy installation.");
            warn!(
                "Omarchy is designed and tested with BTRFS and may not function correctly with ext4."
            );
            if !command.noconfirm {
                let confirmed = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("Are you sure you want to proceed with ext4?")
                    .default(false)
                    .interact()?;
                if !confirmed {
                    return Err(anyhow!(
                        "User aborted due to filesystem mismatch for Omarchy."
                    ));
                }
            }
        // User confirmed, so we leave it as ext4.
        } else {
            if !user_set_fs {
                info!("System variant 'Omarchy' selected. Overriding filesystem to BTRFS.");
            }
            command.filesystem = RootFilesystemType::Btrfs;
        }

        let user_set_aur_helper = env::args().any(|arg| arg.starts_with("--aur-helper"));
        if !user_set_aur_helper {
            info!("Omarchy selected. Defaulting AUR helper to 'yay'.");
            command.aur_helper = crate::aur::AurHelper::Yay;
        }
    }
    Ok(())
}

fn resolve_device_path_and_image(
    command: &CreateCommand,
) -> anyhow::Result<(PathBuf, Option<LoopDevice>)> {
    let storage_device_path = if let Some(path) = &command.path {
        path.clone()
    } else {
        select_block_device(command.allow_non_removable, command.noconfirm)?
    };

    let image_loop = if let Some(size) = command.image {
        Some(create_image(
            &storage_device_path,
            size,
            command.overwrite,
            command.dryrun,
        )?)
    } else {
        None
    };

    let device_path = image_loop
        .as_ref()
        .map(|loop_dev| {
            info!("Using loop device at {}", loop_dev.path().display());
            loop_dev.path().to_path_buf()
        })
        .unwrap_or(storage_device_path);

    Ok((device_path, image_loop))
}

fn select_block_device(allow_non_removable: bool, noconfirm: bool) -> anyhow::Result<PathBuf> {
    if noconfirm {
        return Err(anyhow!(
            "No device path specified. In non-interactive mode, the device path must be provided."
        ));
    }
    let devices = storage::get_storage_devices(allow_non_removable)?;
    if devices.is_empty() {
        return Err(anyhow!("No suitable storage devices found."));
    }
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a device")
        .default(0)
        .items(&devices)
        .interact()?;
    Ok(PathBuf::from("/dev").join(&devices[selection].name))
}

fn create_image(
    path: &Path,
    size: Byte,
    overwrite: bool,
    dryrun: bool,
) -> anyhow::Result<LoopDevice> {
    if !dryrun {
        let mut options = fs::OpenOptions::new();
        options.write(true);
        if overwrite {
            options.create(true);
        } else {
            options.create_new(true);
        }
        let file = options
            .open(path)
            .context("Error creating the image file")?;
        file.set_len(size.as_u64())
            .context("Error setting image file size")?;
    }
    LoopDevice::create(path, dryrun)
}

fn confirm_and_wipe_device(
    storage_device: &mut StorageDevice,
    command: &CreateCommand,
) -> anyhow::Result<()> {
    if storage_device.is_mounted() {
        if !command.noconfirm {
            let confirmed = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!("{} Device {} has mounted partitions. This will unmount them and WIPE ALL DATA. Continue?",
                    style("WARNING:").red().bold(), storage_device.path().display()))
                .default(false).interact()?;
            if !confirmed {
                return Err(anyhow!("User aborted operation."));
            }
        }
        storage_device.umount_if_needed();
    }
    Ok(())
}

fn partition_and_format<'a>(
    command: &CreateCommand,
    tools: &Tools,
    storage_device: &'a StorageDevice,
) -> anyhow::Result<(Option<Partition<'a>>, Partition<'a>)> {
    let default_boot_mb = if command.system == SystemVariant::Omarchy {
        constants::OMARCHY_DEFAULT_BOOT_MB
    } else {
        DEFAULT_BOOT_MB
    };

    let boot_size_mb = command
        .boot_size
        .map_or(default_boot_mb, |b| (b.as_u128() / 1_048_576) as u32);

    if command.system == SystemVariant::Omarchy {
        if boot_size_mb < constants::OMARCHY_MIN_BOOT_MB {
            warn!(
                "The specified boot partition size ({} MiB) is less than the recommended minimum of {} MiB for Omarchy.",
                boot_size_mb,
                constants::OMARCHY_MIN_BOOT_MB
            );
            if !command.noconfirm {
                let confirmed = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("Continuing may cause boot issues. Do you want to proceed?")
                    .default(false)
                    .interact()?;
                if !confirmed {
                    return Err(anyhow!(
                        "User aborted operation due to small boot partition size for Omarchy."
                    ));
                }
            }
        }
    } else if !(MIN_BOOT_MB..=MAX_BOOT_MB).contains(&boot_size_mb) {
        warn!(
            "The specified boot partition size ({boot_size_mb} MiB) is outside the recommended range of {MIN_BOOT_MB} MiB to {MAX_BOOT_MB} MiB."
        );
        warn!(
            "A size that is too small may fail, and a size that is too large is often unnecessary."
        );

        if !command.noconfirm {
            let confirmed = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Do you want to continue with this size?")
                .default(false)
                .interact()?;
            if !confirmed {
                return Err(anyhow!(
                    "User aborted operation due to boot partition size warning."
                ));
            }
        }
    }

    let (boot_partition, root_partition_base) = if let Some(root_partition_path) =
        &command.root_partition
    {
        (
            command
                .boot_partition
                .clone()
                .map(Partition::new::<StorageDevice>),
            Partition::new::<StorageDevice>(root_partition_path.clone()),
        )
    } else {
        let parts = repartition_disk(storage_device, boot_size_mb, &tools.sgdisk, command.dryrun)?;
        (Some(parts.boot_partition), parts.root_partition_base)
    };

    if let Some(bp) = &boot_partition {
        Filesystem::format(bp, FilesystemType::Vfat, &tools.mkfat)?;
    }

    if command.encrypted_root {
        EncryptedDevice::prepare(tools.cryptsetup.as_ref().unwrap(), &root_partition_base)?;
    }

    Ok((boot_partition, root_partition_base))
}

struct DiskPartitions<'a> {
    boot_partition: Partition<'a>,
    root_partition_base: Partition<'a>,
}

fn repartition_disk<'a>(
    storage_device: &'a StorageDevice,
    boot_size_mb: u32,
    sgdisk: &Tool,
    dryrun: bool,
) -> anyhow::Result<DiskPartitions<'a>> {
    info!("Wiping and partitioning the block device");
    sgdisk
        .execute()
        .args([
            "-Z",
            "-o",
            &format!("--new=1::+{boot_size_mb}M"),
            "--new=2::+1M",
            "--largest-new=3",
            "--typecode=1:EF00",
            "--typecode=2:EF02",
        ])
        .arg(storage_device.path())
        .run(dryrun)
        .context("Partitioning error")?;
    std::thread::sleep(std::time::Duration::from_millis(1000));
    Ok(DiskPartitions {
        boot_partition: storage_device.get_partition(constants::BOOT_PARTITION_INDEX)?,
        root_partition_base: storage_device.get_partition(constants::ROOT_PARTITION_INDEX)?,
    })
}

fn bootstrap_system<'a>(
    command: &CreateCommand,
    tools: &Tools,
    boot_filesystem: &'a Option<Filesystem>,
    root_filesystem: &'a Filesystem,
    presets: &PresetsCollection,
    user_settings: Option<&UserSettings>,
) -> anyhow::Result<(tempfile::TempDir, MountStack<'a>)> {
    let mount_point = tempfile::tempdir().context("Error creating a temporary directory")?;
    let mount_stack = mount(
        mount_point.path(),
        boot_filesystem,
        root_filesystem,
        command.dryrun,
    )?;

    let mut packages: HashSet<String> = constants::BASE_PACKAGES
        .iter()
        .map(|s| String::from(*s))
        .collect();

    // Add interactive packages if applicable
    if let Some(settings) = user_settings {
        info!("Adding packages selected during interactive setup...");
        packages.extend(settings.graphics_packages.iter().cloned());
        packages.extend(settings.font_packages.iter().cloned());
    }

    if command.system == SystemVariant::Omarchy {
        info!("Adding Omarchy specific packages (PipeWire, Bluetooth)...");
        packages.extend(
            [
                "wget",
                "gum",
                "pipewire",
                "pipewire-alsa",
                "pipewire-jack",
                "pipewire-pulse",
                "gst-plugin-pipewire",
                "libpulse",
                "wireplumber",
                "bluez",
                "bluez-utils",
                "python",
                "python-gobject",
                "ufw",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    }

    if command.filesystem == RootFilesystemType::Btrfs {
        info!("Adding btrfs-progs for Btrfs filesystem...");
        packages.insert("btrfs-progs".to_string());
    }

    // Add packages from presets and AUR dependencies
    packages.extend(presets.packages.clone());
    packages.extend(constants::AUR_DEPENDENCIES.iter().map(|s| String::from(*s)));

    let pacman_conf_path = command
        .pacman_conf
        .clone()
        .unwrap_or_else(|| "/etc/pacman.conf".into());

    info!("Bootstrapping system");
    tools
        .pacstrap
        .execute()
        .arg("-C")
        .arg(&pacman_conf_path)
        .arg("-c")
        .arg(mount_point.path())
        .args(packages) // The `packages` set now contains all conditional packages
        .args(&command.extra_packages)
        .run(command.dryrun)
        .context("Pacstrap error")?;

    if !command.dryrun {
        fs::copy(pacman_conf_path, mount_point.path().join("etc/pacman.conf"))
            .context("Failed copying pacman.conf")?;
    }

    let fstab = fix_fstab(
        &tools
            .genfstab
            .execute()
            .arg("-U")
            .arg(mount_point.path())
            .run_text_output(command.dryrun)
            .context("fstab error")?,
    );

    if !command.dryrun {
        debug!("fstab:\n{fstab}");
        fs::write(mount_point.path().join("etc/fstab"), fstab).context("fstab error")?;
    };

    tools
        .arch_chroot
        .execute()
        .arg(mount_point.path())
        .args(["passwd", "-d", "root"])
        .run(command.dryrun)
        .context("Failed to delete the root password")?;

    info!("Setting locale");
    if !command.dryrun {
        fs::OpenOptions::new()
            .append(true)
            .open(mount_point.path().join("etc/locale.gen"))
            .and_then(|mut locale_gen| locale_gen.write_all(b"en_US.UTF-8 UTF-8\n"))
            .context("Failed to create locale.gen")?;
        fs::write(
            mount_point.path().join("etc/locale.conf"),
            "LANG=en_US.UTF-8",
        )
        .context("Failed to write to locale.conf")?;
    }
    tools
        .arch_chroot
        .execute()
        .arg(mount_point.path())
        .arg("locale-gen")
        .run(command.dryrun)
        .context("locale-gen failed")?;

    Ok((mount_point, mount_stack))
}

fn bake_sources_into_image(
    tools: &Tools,
    mount_path: &Path,
    presets_paths: &[PathWrapper],
    command: &CreateCommand,
) -> anyhow::Result<()> {
    info!("Baking sources into image for offline installation...");
    let baked_sources_dir = mount_path.join("usr/share/alma/baked_sources");
    if !command.dryrun {
        fs::create_dir_all(&baked_sources_dir)?;
    }
    // Copy presets
    for (i, preset_wrapper) in presets_paths.iter().enumerate() {
        let dest = baked_sources_dir.join(format!("preset_{i}"));
        info!(
            "Copying preset {} to {}",
            command.presets[i],
            dest.display()
        );
        if !command.dryrun {
            fs_extra::dir::copy(
                preset_wrapper.to_path(),
                &dest,
                &fs_extra::dir::CopyOptions::new(),
            )?;
        }
    }
    // Bake Omarchy if needed
    if command.system == SystemVariant::Omarchy {
        let omarchy_baked_path = mount_path.join("usr/share/omarchy");
        info!("Cloning Omarchy repo to bake into image...");
        tools
            .git
            .execute()
            .arg("clone")
            .arg(OMARCHY_REPO_URL)
            .arg(&omarchy_baked_path)
            .run(command.dryrun)?;
    }
    Ok(())
}

fn install_omarchy(
    tools: &Tools,
    mount_path: &Path,
    command: &CreateCommand,
    username: &str,
) -> anyhow::Result<()> {
    info!("Installing Omarchy as user '{username}'...");

    // Define paths
    let user_home_dir_chroot = PathBuf::from("/home").join(username);
    let user_home_dir_host = mount_path.join("home").join(username);
    let target_omarchy_base_dir_host = user_home_dir_host.join(".local/share");
    let install_script_path_chroot = user_home_dir_chroot.join(".local/share/omarchy/install.sh");
    let baked_omarchy_dir = mount_path.join("usr/share/omarchy");

    if !command.dryrun {
        // Ensure the user's home directory exists and copy files.
        // This is a safeguard; `useradd -m` should have created the home dir.
        fs::create_dir_all(&user_home_dir_host)?;
        fs::create_dir_all(&target_omarchy_base_dir_host)?;

        let mut copy_options = fs_extra::dir::CopyOptions::new();
        copy_options.overwrite = true;
        fs_extra::dir::copy(
            &baked_omarchy_dir,
            &target_omarchy_base_dir_host,
            &copy_options,
        )?;

        // Copy firewall.sh to user home dir
        let firewall_src_path = target_omarchy_base_dir_host
            .join("omarchy")
            .join("install")
            .join("development")
            .join("firewall.sh");
        let firewall_dest_path = user_home_dir_host.join("firewall.sh");
        info!("Copying firewall.sh to user's home directory.");
        fs::copy(&firewall_src_path, &firewall_dest_path)?;

        info!("Setting ownership for user '{username}'");

        tools
            .arch_chroot
            .execute()
            .arg(mount_path)
            .args([
                "chown",
                "-R",
                &format!("{username}:{username}"),
                user_home_dir_chroot.to_str().unwrap(),
            ])
            .run(command.dryrun)?;
    } else {
        println!(
            "cp -r {} {}",
            baked_omarchy_dir.display(),
            target_omarchy_base_dir_host.display()
        );
        let firewall_src_path = target_omarchy_base_dir_host
            .join("omarchy")
            .join("install")
            .join("development")
            .join("firewall.sh");
        let firewall_dest_path = user_home_dir_host.join("firewall.sh");
        println!(
            "cp {} {}",
            firewall_src_path.display(),
            firewall_dest_path.display()
        );
    }

    info!("Patching Omarchy scripts to remove systemctl '--now' flag...");
    let patch_command = format!(
        "find /home/{username}/.local/share/omarchy -type f -name '*.sh' -print0 | xargs -0 sed -i \
            -e 's/enable --now/enable/g' \
            -e 's/sudo ufw enable/sudo systemctl enable ufw.service/g' \
            -e 's/^reboot/# reboot (disabled in chroot)/g' \
            -e 's/sudo ufw reload/# sudo ufw reload (disabled in chroot)/g'",
    );

    let ufw_path = mount_path.join("usr/bin/ufw");
    let ufw_real_path = mount_path.join("usr/bin/ufw.real");

    let wrapper_script = r#"#!/bin/bash
echo "[alma-nv wrapper] Intercepted ufw command: ufw $@" >&2
if [[ "$1" == "enable" ]]; then
  echo "[alma-nv wrapper] Executing 'systemctl enable ufw.service' instead." >&2
  systemctl enable ufw.service
else
  echo "[alma-nv wrapper] Suppressing stateful ufw command in chroot." >&2
fi
exit 0
"#;

    // 1. Rename the real ufw and create the wrapper
    info!("Wrapping ufw command to make it chroot-safe...");
    if !command.dryrun {
        if ufw_path.exists() {
            fs::rename(&ufw_path, &ufw_real_path).context("Failed to move real ufw binary")?;
            fs::write(&ufw_path, wrapper_script).context("Failed to write ufw wrapper script")?;
            fs::set_permissions(
                &ufw_path,
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )?;
        }
    } else if command.dryrun {
        println!("mv {} {}", ufw_path.display(), ufw_real_path.display());
        println!(
            "echo '...' > {} && chmod 755 {}",
            ufw_path.display(),
            ufw_path.display()
        );
    }

    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .args(["bash", "-c", &patch_command])
        .run(command.dryrun)
        .context("Failed to patch Omarchy install scripts.")?;

    info!("Running patched Omarchy install script as user '{username}'. This will be interactive.");

    // Use `sudo -u` to run the command as the specified user.
    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .args([
            "sudo",
            "-u",
            username,
            "bash",
            install_script_path_chroot.to_str().unwrap(),
        ])
        .run(command.dryrun)
        .context("Omarchy installation script failed.")?;

    info!("Restoring original ufw command...");
    if !command.dryrun && ufw_real_path.exists() {
        fs::rename(&ufw_real_path, &ufw_path).context("Failed to restore real ufw binary")?;
    } else if command.dryrun {
        println!("mv {} {}", ufw_real_path.display(), ufw_path.display());
    }

    Ok(())
}

fn generate_manifest(
    command: &CreateCommand,
    mount_point: &tempfile::TempDir,
    original_command: &str,
    sources: &mut Vec<Source>,
) -> anyhow::Result<()> {
    info!("Generating installation manifest...");
    if command.system == SystemVariant::Omarchy {
        sources.push(Source {
            r#type: "system".to_string(),
            origin: OMARCHY_REPO_URL.to_string(),
            baked_path: PathBuf::from("/usr/share/omarchy"),
        });
    }

    let manifest = Manifest {
        alma_version: env!("CARGO_PKG_VERSION").to_string(),
        system_variant: command.system,
        filesystem: command.filesystem,
        encrypted_root: command.encrypted_root,
        aur_helper: command.aur_helper.to_string(),
        original_command: original_command.to_string(),
        sources: std::mem::take(sources),
    };

    let manifest_path = mount_point.path().join("usr/share/alma/manifest.json");
    if !command.dryrun {
        let json = serde_json::to_string_pretty(&manifest)?;
        fs::write(manifest_path, json)?;
    }
    Ok(())
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
        debug!("Root partition UUID: {trimmed}");

        if !dryrun {
            let mut grub_file = fs::OpenOptions::new()
                .append(true)
                .open(mount_point.path().join("etc/default/grub"))
                .context("Failed to create /etc/default/grub")?;

            // TODO: Handle multiple encrypted partitions with osprober?
            write!(
                &mut grub_file,
                "GRUB_CMDLINE_LINUX=\"cryptdevice=UUID={trimmed}:luks_root\""
            )
            .context("Failed to write to /etc/default/grub")?;
        }
    }

    // TODO: add grub os-prober?
    // TODO: Allow choice of bootloader - systemd-boot + refind?
    // TODO: Add systemd volatile root option

    info!("Enabling os-prober for multi-boot detection");
    if !dryrun {
        let grub_conf_path = mount_point.path().join("etc/default/grub");
        let mut grub_conf = fs::read_to_string(&grub_conf_path)?;

        // Ensure GRUB_DISABLE_OS_PROBER is false and add required options for os-prober
        grub_conf = grub_conf.replace(
            "GRUB_DISABLE_OS_PROBER=true",
            "GRUB_DISABLE_OS_PROBER=false",
        );

        // Add or ensure that os-prober is enabled in the grub configuration
        // We're just adding a standard configuration line.
        if !grub_conf.contains("GRUB_CMDLINE_LINUX") {
            grub_conf.push_str("\nGRUB_CMDLINE_LINUX=\"\"\n");
        }

        fs::write(grub_conf_path, grub_conf)?;
    }

    info!("Installing the Bootloader");
    run_grub_mkconfig_scoped(storage_device, mount_point, arch_chroot, dryrun)?;

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

fn apply_customizations(
    command: &CreateCommand,
    arch_chroot: &Tool,
    presets: &PresetsCollection,
    mount_path: &Path,
) -> anyhow::Result<()> {
    // Install AUR helper and packages
    info!("Installing AUR packages");
    let aur_packages = {
        let mut p = vec![String::from("shim-signed")];
        p.extend(presets.aur_packages.clone());
        p.extend(command.aur_packages.clone());
        p
    };

    if !aur_packages.is_empty() {
        arch_chroot
            .execute()
            .arg(mount_path)
            .args(["useradd", "-m", "aur"])
            .run(command.dryrun)
            .context("Failed to create temporary user to install AUR packages")?;

        let aur_sudoers = mount_path.join("etc/sudoers.d/aur");
        if !command.dryrun {
            fs::write(&aur_sudoers, "aur ALL=(ALL) NOPASSWD: ALL")
                .context("Failed to modify sudoers file for AUR packages")?;
        }

        arch_chroot
            .execute()
            .arg(mount_path)
            .args(["sudo", "-u", "aur"])
            .arg("git")
            .arg("clone")
            .arg(format!(
                "https://aur.archlinux.org/{}.git",
                &command.aur_helper.get_package_name()
            ))
            .arg(format!("/home/aur/{}", &command.aur_helper.to_string()))
            .run(command.dryrun)
            .context("Failed to clone AUR helper package")?;

        arch_chroot
            .execute()
            .arg(mount_path)
            .args([
                "bash",
                "-c",
                &format!(
                    "cd /home/aur/{} && sudo -u aur makepkg -s -i --noconfirm",
                    &command.aur_helper.to_string()
                ),
            ])
            .run(command.dryrun)
            .context("Failed to build AUR helper")?;

        arch_chroot
            .execute()
            .arg(mount_path)
            .args(["sudo", "-u", "aur"])
            .args(command.aur_helper.get_install_command())
            .args(aur_packages)
            .run(command.dryrun)
            .context("Failed to install AUR packages")?;

        // Clean up aur user:
        arch_chroot
            .execute()
            .arg(mount_path)
            .args(["userdel", "-r", "aur"])
            .run(command.dryrun)
            .context("Failed to delete temporary aur user")?;

        if !command.dryrun {
            fs::remove_file(&aur_sudoers)
                .context("Cannot delete the AUR sudoers temporary file")?;
        }
    }

    // Run preset scripts
    if !presets.scripts.is_empty() {
        info!("Running custom scripts");
    }

    for script in &presets.scripts {
        run_preset_script(command, arch_chroot, script, mount_path)?;
    }

    Ok(())
}

fn run_preset_script(
    command: &CreateCommand,
    arch_chroot: &Tool,
    script: &Script,
    mount_path: &Path,
) -> anyhow::Result<()> {
    let mut bind_mount_stack = MountStack::new(command.dryrun);
    if let Some(shared_dirs) = &script.shared_dirs {
        for dir in shared_dirs {
            let shared_dirs_path = mount_path
                .join(PathBuf::from("shared_dirs/"))
                .join(dir.file_name().expect("Dir had no filename"));

            if !command.dryrun {
                std::fs::create_dir_all(&shared_dirs_path)
                    .context("Failed mounting shared directories in preset")?;
            } else {
                println!("mkdir -p {}", shared_dirs_path.display());
            }

            bind_mount_stack
                .bind_mount(dir.clone(), shared_dirs_path, None)
                .context("Failed mounting shared directories in preset")?;
        }
    }

    let mut script_file = tempfile::NamedTempFile::new_in(mount_path)
        .context("Failed creating temporary preset script")?;
    script_file
        .write_all(script.script_text.as_bytes())
        .and_then(|_| script_file.as_file_mut().metadata())
        .and_then(|metadata| {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(script_file.path(), permissions)
        })
        .context("Failed creating temporary preset script")?;

    let script_path_in_chroot = Path::new("/").join(
        script_file
            .path()
            .file_name()
            .expect("Script path had no file name"),
    );

    arch_chroot
        .execute()
        .arg(mount_path)
        .arg(script_path_in_chroot)
        .run(command.dryrun)
        .with_context(|| format!("Failed running preset script:\n{}", script.script_text))?;

    Ok(())
}

fn finalize_installation(
    command: &CreateCommand,
    tools: &Tools,
    storage_device: &StorageDevice,
    mount_point: &TempDir,
    encrypted_root: Option<&EncryptedDevice>,
    root_partition_base: &Partition,
) -> anyhow::Result<()> {
    info!("Performing post installation tasks");

    tools
        .arch_chroot
        .execute()
        .arg(mount_point.path())
        .args(["systemctl", "enable", "NetworkManager"])
        .run(command.dryrun)
        .context("Failed to enable NetworkManager")?;

    info!("Configuring journald");
    if !command.dryrun {
        fs::write(
            mount_point.path().join("etc/systemd/journald.conf"),
            constants::JOURNALD_CONF,
        )
        .context("Failed to write to journald.conf")?;
    }

    // Only set up bootloader if boot partition is mounted
    if command.root_partition.is_none() || command.boot_partition.is_some() {
        setup_bootloader(
            storage_device,
            mount_point,
            &tools.arch_chroot,
            encrypted_root,
            root_partition_base,
            tools.blkid.as_ref(),
            command.dryrun,
        )?;
    }

    Ok(())
}

fn interactive_chroot_and_cleanup(
    command: &CreateCommand,
    arch_chroot: &Tool,
    mount_path: &Path,
    mount_stack: MountStack,
) -> anyhow::Result<()> {
    if command.interactive && !command.dryrun {
        info!(
            "Dropping you to chroot. Do as you wish to customize the installation. Please exit by typing 'exit' instead of using Ctrl+D"
        );
        arch_chroot
            .execute()
            .arg(mount_path)
            .run(false)
            .context("Failed to enter interactive chroot")?;
    }

    info!("Unmounting filesystems");
    mount_stack.umount()?;

    Ok(())
}

fn run_script_in_chroot(
    script_text: &str,
    arch_chroot: &Tool,
    mount_path: &Path,
    dryrun: bool,
) -> anyhow::Result<()> {
    // The tempfile logic was slightly flawed, this is the most direct way
    let temp_file_obj = tempfile::Builder::new()
        .prefix(".")
        .tempfile_in(mount_path)?;

    // 1. Write content.
    temp_file_obj.as_file().write_all(script_text.as_bytes())?;
    temp_file_obj.as_file().sync_all()?;

    // 2. Persist to close the handle.
    let temp_path = temp_file_obj.into_temp_path();

    // 3. Set permissions on the now-closed file.
    let mut perms = fs::metadata(&temp_path)?.permissions();
    perms.set_mode(0o755); // This now works because PermissionsExt is in scope
    fs::set_permissions(&temp_path, perms)?; // This now works because `perms` is the right type

    let script_path_in_chroot =
        Path::new("/").join(temp_path.file_name().expect("Script path had no file name"));

    // 4. Execute the script.
    let result = arch_chroot
        .execute()
        .arg(mount_path)
        .arg(script_path_in_chroot.to_str().unwrap())
        .run(dryrun);

    // 5. Manually clean up the file (TempPath cleans itself on drop, but explicit is fine)
    if let Err(e) = temp_path.close() {
        log::warn!("Failed to clean up temporary script file: {e}");
    }

    result.with_context(|| format!("Failed running setup script:\n{script_text}"))
}

/// Runs grub-mkconfig with os-prober temporarily wrapped to only scan the target device.
fn run_grub_mkconfig_scoped(
    storage_device: &StorageDevice,
    mount_point: &tempfile::TempDir,
    arch_chroot: &Tool,
    dryrun: bool,
) -> anyhow::Result<()> {
    info!("Installing GRUB and running scoped os-prober...");

    let disk_path = storage_device.path();
    let os_prober_path = mount_point.path().join("usr/bin/os-prober");
    let os_prober_real_path = mount_point.path().join("usr/bin/os-prober.real");

    // The wrapper script that limits os-prober's scope
    let wrapper_script = format!(
        "#!/bin/sh\nexport OS_PROBER_DEVICES=\"{}\"\nexec /usr/bin/os-prober.real \"$@\"\n",
        disk_path.display()
    );

    // 1. Rename the real os-prober
    info!(
        "Wrapping os-prober to limit scan to {}",
        disk_path.display()
    );
    if !dryrun && os_prober_path.exists() {
        fs::rename(&os_prober_path, &os_prober_real_path)
            .context("Failed to move real os-prober")?;
    } else if dryrun {
        println!(
            "mv {} {}",
            os_prober_path.display(),
            os_prober_real_path.display()
        );
    }

    // 2. Write and chmod the wrapper script
    if !dryrun && os_prober_real_path.exists() {
        fs::write(&os_prober_path, &wrapper_script)
            .context("Failed to write os-prober wrapper script")?;
        fs::set_permissions(
            &os_prober_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )?;
    } else if dryrun {
        println!("echo '{}' > {}", wrapper_script, os_prober_path.display());
        println!("chmod 755 {}", os_prober_path.display());
    }

    // 3. Run grub-install and grub-mkconfig
    let result = arch_chroot.execute()
        .arg(mount_point.path())
        .args(["bash", "-c"])
        .arg(format!(
            "grub-install --target=i386-pc --boot-directory /boot {0} && \
             grub-install --target=x86_64-efi --efi-directory /boot --boot-directory /boot --removable {0} && \
             grub-mkconfig -o /boot/grub/grub.cfg",
            disk_path.display()
        ))
        .run(dryrun);

    // 4. Clean up: restore the real os-prober, regardless of the result
    info!("Unwrapping os-prober...");
    if !dryrun && os_prober_real_path.exists() {
        fs::rename(&os_prober_real_path, &os_prober_path)
            .context("Failed to restore real os-prober")?;
    } else if dryrun {
        println!(
            "mv {} {}",
            os_prober_real_path.display(),
            os_prober_path.display()
        );
    }

    result.context("Failed to install grub or run grub-mkconfig")
}
