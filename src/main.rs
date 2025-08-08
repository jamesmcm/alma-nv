mod args;
mod aur;
mod constants;
mod create;
mod initcpio;
mod presets;
mod process;
mod storage;
mod tool;

use anyhow::{Context, anyhow};
use args::{Command, CreateCommand};
use byte_unit::Byte;
use clap::Parser;
use console::style;
use dialoguer::{Confirm, Select, theme::ColorfulTheme};
use log::{LevelFilter, debug, info, warn};
use presets::{PresetsCollection, Script};
use process::CommandExt;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use storage::EncryptedDevice;
use storage::{BlockDevice, Filesystem, FilesystemType, LoopDevice, MountStack};
use tempfile::{TempDir, tempdir};
use tool::Tool;

use crate::constants::{DEFAULT_BOOT_MB, MAX_BOOT_MB, MIN_BOOT_MB};
use crate::create::setup_bootloader;
use crate::presets::PathWrapper;
use crate::storage::StorageDevice;
use crate::storage::partition::Partition;

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
fn select_block_device(allow_non_removable: bool, noconfirm: bool) -> anyhow::Result<PathBuf> {
    if noconfirm {
        return Err(anyhow!(
            "No device path specified. In non-interactive mode (--noconfirm), the device path must be provided as an argument."
        ));
    }

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

struct Tools {
    sgdisk: Tool,
    pacstrap: Tool,
    arch_chroot: Tool,
    genfstab: Tool,
    mkfat: Tool,
    mkext4: Tool,
    cryptsetup: Option<Tool>,
    blkid: Option<Tool>,
}

impl Tools {
    fn new(command: &CreateCommand) -> anyhow::Result<Self> {
        let dryrun = command.dryrun;
        let encrypted = command.encrypted_root;

        Ok(Self {
            sgdisk: Tool::find("sgdisk", dryrun)?,
            pacstrap: Tool::find("pacstrap", dryrun)?,
            arch_chroot: Tool::find("arch-chroot", dryrun)?,
            genfstab: Tool::find("genfstab", dryrun)?,
            mkfat: Tool::find("mkfs.fat", dryrun)?,
            mkext4: Tool::find("mkfs.ext4", dryrun)?,
            cryptsetup: if encrypted {
                Some(Tool::find("cryptsetup", dryrun)?)
            } else {
                None
            },
            blkid: if encrypted {
                Some(Tool::find("blkid", dryrun)?)
            } else {
                None
            },
        })
    }
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

fn bootstrap_system<'a>(
    command: &CreateCommand,
    tools: &Tools,
    boot_filesystem: &'a Option<Filesystem>,
    root_filesystem: &'a Filesystem,
    presets: &PresetsCollection,
) -> anyhow::Result<(TempDir, MountStack<'a>)> {
    let mount_point = tempdir().context("Error creating a temporary directory")?;
    let mount_stack = tool::mount(
        mount_point.path(),
        boot_filesystem,
        root_filesystem,
        command.dryrun,
    )?;

    let mut packages: HashSet<String> = constants::BASE_PACKAGES
        .iter()
        .map(|s| String::from(*s))
        .collect();
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
        .args(packages)
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

/// Creates the installation
fn create(command: CreateCommand) -> anyhow::Result<()> {
    // 1. Load presets. We do this first to validate environment variables.
    let presets_paths: Vec<PathWrapper> = command
        .presets
        .clone()
        .into_iter()
        .map(|p| {
            p.into_path_wrapper(command.noconfirm).map_err(|e| {
                log::error!("Error reading preset: {e}");
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

    // 2. Prepare tools
    let tools = Tools::new(&command)?;

    // 3. Resolve device path and create loop device if needed
    let (storage_device_path, _image_loop) = resolve_device_path_and_image(&command)?;
    let mut storage_device = StorageDevice::from_path(
        &storage_device_path,
        command.allow_non_removable,
        command.dryrun,
    )?;

    // 4. Critical safety check: unmount if needed, with confirmation
    if storage_device.is_mounted() {
        if !command.noconfirm {
            let confirmed = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "{} Device {} has mounted partitions. Proceeding will unmount them and WIPE ALL DATA on the device. Are you sure?",
                    style("WARNING:").red().bold(),
                    storage_device.path().display()
                ))
                .default(false)
                .interact()?;
            if !confirmed {
                return Err(anyhow!("User aborted operation."));
            }
        }
        storage_device.umount_if_needed();
    }

    // 5. Partition disk, format filesystems, and set up encryption
    // This logic must be in the main `create` scope to manage lifetimes correctly.
    let boot_size_mb = command
        .boot_size
        .map_or(DEFAULT_BOOT_MB, |b| (b.as_u128() / 1_048_576) as u32);

    // --- New boot size validation ---

    if !(MIN_BOOT_MB..=MAX_BOOT_MB).contains(&boot_size_mb) {
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
    // --- End validation ---

    let (boot_partition, root_partition_base) = if let Some(root_partition_path) =
        command.root_partition.clone()
    {
        // Logic check: if using custom partitions, user must specify boot partition to get a bootloader.
        if command.boot_partition.is_none() {
            warn!(
                "A custom --root-partition was specified, but --boot-partition was not. The bootloader will not be installed."
            );
        }
        (
            command
                .boot_partition
                .clone()
                .map(Partition::new::<StorageDevice>),
            Partition::new::<StorageDevice>(root_partition_path),
        )
    } else {
        let disk_partitions =
            create::repartition_disk(&storage_device, boot_size_mb, &tools.sgdisk, command.dryrun)?;
        (
            Some(disk_partitions.boot_partition),
            disk_partitions.root_partition_base,
        )
    };

    // Format boot partition
    if let Some(bp) = &boot_partition {
        Filesystem::format(bp, FilesystemType::Vfat, &tools.mkfat)?;
    }

    // Prepare encryption on the base root partition if requested
    if command.encrypted_root {
        if command.noconfirm {
            return Err(anyhow!(
                "Non-interactive encrypted root setup is not supported. The passphrase must be entered manually."
            ));
        }
        EncryptedDevice::prepare(tools.cryptsetup.as_ref().unwrap(), &root_partition_base)?;
    }

    // Open the encrypted container. This object's lifetime is managed here.
    // Its `drop` implementation will automatically close the container.
    let encrypted_root = if command.encrypted_root {
        Some(EncryptedDevice::open(
            tools.cryptsetup.as_ref().unwrap(),
            &root_partition_base,
            "alma_root".into(),
        )?)
    } else {
        None
    };

    // Determine the correct block device for the root filesystem and format it
    // We coerce to a trait object here, which is safe.
    let root_block_device: &dyn BlockDevice = if let Some(e) = &encrypted_root {
        e
    } else {
        &root_partition_base
    };
    Filesystem::format(root_block_device, FilesystemType::Ext4, &tools.mkext4)?;

    // From here, we create Filesystem handles which borrow the state we just created.
    let boot_filesystem = boot_partition
        .as_ref()
        .map(|p| Filesystem::from_partition(p, FilesystemType::Vfat));

    let root_filesystem = Filesystem::from_partition(root_block_device, FilesystemType::Ext4);

    // 6. Bootstrap system
    let (mount_point, mount_stack) = bootstrap_system(
        &command,
        &tools,
        &boot_filesystem,
        &root_filesystem,
        &presets,
    )?;

    // 7. Apply all customizations (AUR packages, preset scripts)
    apply_customizations(&command, &tools.arch_chroot, &presets, mount_point.path())?;

    // 8. Finalize installation (bootloader, services)
    finalize_installation(
        &command,
        &tools,
        &storage_device,
        &mount_point,
        encrypted_root.as_ref(),
        &root_partition_base,
    )?;

    // 9. Interactive chroot and cleanup
    interactive_chroot_and_cleanup(
        &command,
        &tools.arch_chroot,
        mount_point.path(),
        mount_stack,
    )?;

    info!("Installation complete!");
    Ok(())
}
