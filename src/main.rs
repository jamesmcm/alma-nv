mod args;
mod aur;
mod constants;
mod create;
mod initcpio;
mod presets;
mod process;
mod storage;
mod tool;

use anyhow::{anyhow, Context};
use args::Command;
use byte_unit::Byte;
use clap::Parser;
use console::style;
use dialoguer::{theme::ColorfulTheme, Select};
use log::{debug, info, LevelFilter};
use process::CommandExt;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use storage::EncryptedDevice;
use storage::{BlockDevice, Filesystem, FilesystemType, LoopDevice, MountStack};
use tempfile::tempdir;
use tool::Tool;

use crate::create::{setup_bootloader, DiskPartitions};
use crate::presets::PathWrapper;
use crate::storage::partition::Partition;
use crate::storage::StorageDevice;

fn main() -> anyhow::Result<()> {
    // Get struct of args using clap
    let app = args::App::parse();

    // Set up logging
    let mut builder = pretty_env_logger::formatted_timed_builder();
    let log_level = if app.verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    builder.filter_level(log_level);
    builder.init();

    // Match command from arguments and run relevant code
    match app.cmd {
        Command::Create(command) => create(command),
        Command::Chroot(command) => tool::chroot(command),
        Command::Qemu(command) => tool::qemu(command),
    }?;

    Ok(())
}

/// Remove swap entry from fstab and any commented lines
/// Returns an owned String
///
/// # Arguments
/// * `fstab` - A string slice holding the contents of the fstab file
fn fix_fstab(fstab: &str) -> String {
    fstab
        .lines()
        .filter(|line| !line.contains("swap") && !line.starts_with('#'))
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Creates a file at the path provided, and mounts it to a loop device
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
        let file = options.open(path).context("Error creating the image")?;

        file.set_len(size.as_u64())
            .context("Error creating the image")?;
    }

    LoopDevice::create(path, dryrun)
}

/// Requests selection of block device (no device was given in the arguments)
fn select_block_device(allow_non_removable: bool) -> anyhow::Result<PathBuf> {
    let devices = storage::get_storage_devices(allow_non_removable)?;

    if devices.is_empty() {
        return Err(anyhow!("There are no removable devices"));
    }

    if allow_non_removable {
        println!(
            "{}\n",
            style("Showing non-removable devices. Make sure you select the correct device.")
                .red()
                .bold()
        );
    }

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a removable device")
        .default(0)
        .items(&devices)
        .interact()?;

    Ok(PathBuf::from("/dev").join(&devices[selection].name))
}

/// Creates the installation
#[allow(clippy::cognitive_complexity)] // TODO: Split steps into functions and remove this
fn create(command: args::CreateCommand) -> anyhow::Result<()> {
    // We fail on any failed preset deliberately
    let presets_paths: Vec<PathWrapper> = command
        .presets
        .into_iter()
        .map(|p| {
            p.into_path_wrapper().map_err(|e| {
                log::error!("Error reading preset: {}", e);
                e
            })
        })
        .collect::<anyhow::Result<Vec<PathWrapper>>>()?;

    let presets = presets::PresetsCollection::load(
        &presets_paths
            .iter()
            .map(|x| x.to_path())
            .collect::<Vec<&Path>>(),
    )?;

    let sgdisk = Tool::find("sgdisk", command.dryrun)?;
    let pacstrap = Tool::find("pacstrap", command.dryrun)?;
    let arch_chroot = Tool::find("arch-chroot", command.dryrun)?;
    let genfstab = Tool::find("genfstab", command.dryrun)?;
    let mkfat = Tool::find("mkfs.fat", command.dryrun)?;
    // TODO: btrfs support
    let mkext4 = Tool::find("mkfs.ext4", command.dryrun)?;

    // TODO: Support separate home partition and encryption of only that
    // https://wiki.archlinux.org/title/dm-crypt/Encrypting_a_non-root_file_system

    let cryptsetup = if command.encrypted_root {
        Some(Tool::find("cryptsetup", command.dryrun)?)
    } else {
        None
    };
    let blkid = if command.encrypted_root {
        Some(Tool::find("blkid", command.dryrun)?)
    } else {
        None
    };

    let storage_device_path = if let Some(path) = command.path {
        path
    } else {
        select_block_device(command.allow_non_removable)?
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

    debug!("Creating StorageDevice");
    let mut storage_device = storage::StorageDevice::from_path(
        image_loop
            .as_ref()
            .map(|loop_dev| {
                info!("Using loop device at {}", loop_dev.path().display());
                loop_dev.path()
            })
            .unwrap_or(&storage_device_path),
        command.allow_non_removable,
        command.dryrun,
    )?;
    debug!("Created StorageDevice");

    // TODO: Warn and prompt if unmounting, as could mean wrong disk chosen
    if !command.dryrun {
        storage_device.umount_if_needed();
    }
    let boot_size = command.boot_size.unwrap_or(500);

    let (boot_partition, root_partition_base) =
        if let Some(root_partition_path) = command.root_partition {
            let root_partition_base = Partition::new::<StorageDevice>(root_partition_path);
            (
                command.boot_partition.map(Partition::new::<StorageDevice>),
                root_partition_base,
            )
        } else {
            let DiskPartitions {
                boot_partition,
                root_partition_base,
            } = create::repartition_disk(&storage_device, boot_size, &sgdisk, command.dryrun)?;

            (Some(boot_partition), root_partition_base)
        };

    let boot_filesystem: Option<Filesystem> = boot_partition
        .as_ref()
        .map(|bp| Filesystem::format(bp, FilesystemType::Vfat, &mkfat))
        .transpose()?;

    let encrypted_root = if let Some(cryptsetup) = &cryptsetup {
        info!("Encrypting the root filesystem");
        EncryptedDevice::prepare(cryptsetup, &root_partition_base)?;
        Some(EncryptedDevice::open(
            cryptsetup,
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

    let root_filesystem = Filesystem::format(root_partition, FilesystemType::Ext4, &mkext4)?;

    // TODO: If given partitions then read in here - do we need boot_partition?
    let mount_point = tempdir().context("Error creating a temporary directory")?;
    let mount_stack = tool::mount(
        mount_point.path(),
        &boot_filesystem,
        &root_filesystem,
        command.dryrun,
    )?;

    // if log_enabled!(Level::Debug) {
    //     debug!("lsblk:");
    //     ProcessCommand::new("lsblk")
    //         .arg("--fs")
    //         .spawn()
    //         .and_then(|mut p| p.wait())
    //         .map_err(|e| {
    //             error!("Error running lsblk: {}", e);
    //         })
    //         .ok();
    // }

    let mut packages: HashSet<String> = constants::BASE_PACKAGES
        .iter()
        .map(|s| String::from(*s))
        .collect();

    packages.extend(presets.packages);

    let aur_pacakges = {
        let mut p = vec![String::from("shim-signed")];
        p.extend(presets.aur_packages);
        p.extend(command.aur_packages);
        p
    };

    packages.extend(constants::AUR_DEPENDENCIES.iter().map(|s| String::from(*s)));

    let pacman_conf_path = command
        .pacman_conf
        .unwrap_or_else(|| "/etc/pacman.conf".into());

    info!("Bootstrapping system");
    pacstrap
        .execute()
        .arg("-C")
        .arg(&pacman_conf_path)
        .arg("-c")
        .arg(mount_point.path())
        .args(packages)
        .args(&command.extra_packages)
        .run(command.dryrun)
        .context("Pacstrap error")?;

    // Copy pacman.conf to the image.
    if !command.dryrun {
        fs::copy(pacman_conf_path, mount_point.path().join("etc/pacman.conf"))
            .context("Failed copying pacman.conf")?;
    }

    let fstab = fix_fstab(
        &genfstab
            .execute()
            .arg("-U")
            .arg(mount_point.path())
            .run_text_output(command.dryrun)
            .context("fstab error")?,
    );

    if !command.dryrun {
        debug!("fstab:\n{}", fstab);
        fs::write(mount_point.path().join("etc/fstab"), fstab).context("fstab error")?;
    };
    arch_chroot
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
    arch_chroot
        .execute()
        .arg(mount_point.path())
        .arg("locale-gen")
        .run(command.dryrun)
        .context("locale-gen failed")?;

    info!("Installing AUR packages");

    arch_chroot
        .execute()
        .arg(mount_point.path())
        .args(["useradd", "-m", "aur"])
        .run(command.dryrun)
        .context("Failed to create temporary user to install AUR packages")?;

    let aur_sudoers = mount_point.path().join("etc/sudoers.d/aur");
    if !command.dryrun {
        fs::write(&aur_sudoers, "aur ALL=(ALL) NOPASSWD: ALL")
            .context("Failed to modify sudoers file for AUR packages")?;
    }

    arch_chroot
        .execute()
        .arg(mount_point.path())
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
        .arg(mount_point.path())
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
        .arg(mount_point.path())
        .args(["sudo", "-u", "aur"])
        .args(&command.aur_helper.get_install_command())
        .args(aur_pacakges)
        .run(command.dryrun)
        .context("Failed to install AUR packages")?;

    // Clean up aur user:
    arch_chroot
        .execute()
        .arg(mount_point.path())
        .args(["userdel", "-r", "aur"])
        .run(command.dryrun)
        .context("Failed to delete temporary aur user")?;

    if !command.dryrun {
        fs::remove_file(&aur_sudoers).context("Cannot delete the AUR sudoers temporary file")?;
    }

    if !presets.scripts.is_empty() {
        info!("Running custom scripts");
    }

    // Recursively copy preset scripts to the chroot mount in /usr/share/alma/presets/{preset_index}/
    let options = fs_extra::dir::CopyOptions::new().copy_inside(true);
    for (i, path) in presets_paths.iter().enumerate() {
        let target = mount_point
            .path()
            .join(format!("usr/share/alma/presets/{}/", i));
        fs_extra::copy_items(&[path.to_path()], target, &options)?;
    }

    for script in presets.scripts {
        let mut bind_mount_stack = MountStack::new(command.dryrun);
        if let Some(shared_dirs) = &script.shared_dirs {
            for dir in shared_dirs {
                // Create shared directories mount points inside chroot
                let shared_dirs_path = mount_point
                    .path()
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

        let mut script_file = tempfile::NamedTempFile::new_in(mount_point.path())
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

        let script_path = script_file.into_temp_path();
        arch_chroot
            .execute()
            .arg(mount_point.path())
            .arg(
                Path::new("/").join(
                    script_path
                        .file_name()
                        .expect("Script path had no file name"),
                ),
            )
            .run(command.dryrun)
            .with_context(|| format!("Failed running preset script:\n{}", script.script_text))?;
    }

    info!("Performing post installation tasks");

    arch_chroot
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

    setup_bootloader(
        &storage_device,
        &mount_point,
        &arch_chroot,
        encrypted_root.as_ref(),
        &root_partition_base,
        blkid.as_ref(),
        command.dryrun,
    )?;

    if command.interactive && !command.dryrun {
        info!("Dropping you to chroot. Do as you wish to customize the installation. Please exit by typing 'exit' instead of using Ctrl+D");
        arch_chroot
            .execute()
            .arg(mount_point.path())
            .run(false)
            .context("Failed to enter interactive chroot")?;
    }

    info!("Unmounting filesystems");
    mount_stack.umount()?;

    Ok(())
}
