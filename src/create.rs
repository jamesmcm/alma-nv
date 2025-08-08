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

use crate::args::{CreateCommand, FilesystemTypeArg, Manifest, Source, SystemVariant};
use crate::constants::{self, OMARCHY_REPO_URL};
use crate::initcpio;
use crate::interactive::UserSettings;
use crate::presets::{PathWrapper, PresetsCollection, Script};
use crate::process::CommandExt;
use crate::storage::{
    BlockDevice, EncryptedDevice, Filesystem, FilesystemType, LoopDevice, MountStack,
    StorageDevice, partition::Partition,
};
use crate::tool::{Tool, mount};

pub struct Tools {
    sgdisk: Tool,
    pacstrap: Tool,
    arch_chroot: Tool,
    genfstab: Tool,
    mkfat: Tool,
    mkext4: Tool,
    mkbtrfs: Tool,
    btrfs: Tool, // New tool
    git: Tool,
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
            mkbtrfs: Tool::find("mkfs.btrfs", dryrun)?,
            btrfs: Tool::find("btrfs", dryrun)?, // Find the btrfs utility
            git: Tool::find("git", dryrun)?,
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

pub fn create(mut command: CreateCommand) -> anyhow::Result<()> {
    // --- Initial Command Validation & Adjustments ---
    validate_command(&command)?;
    adjust_command_for_system(&mut command);

    let original_command_string = env::args().collect::<Vec<String>>().join(" ");
    let mut manifest_sources: Vec<Source> = Vec::new();

    // 1. Load presets. We do this first to validate environment variables.
    let presets_paths = command
        .presets
        .clone()
        .into_iter()
        .map(|p| p.into_path_wrapper(command.noconfirm))
        .collect::<anyhow::Result<Vec<PathWrapper>>>()?;

    for (i, p_path) in presets_paths.iter().enumerate() {
        let origin_path = command.presets[i].to_string();
        let baked_path =
            PathBuf::from("/usr/share/alma/baked_sources").join(format!("preset_{}", i));
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

    let user_settings = if command.noconfirm {
        // Handle non-interactive case later, for now, assume environment variables are set.
        // If they aren't, the presets will fail.
        None
    } else {
        Some(UserSettings::prompt(command.noconfirm)?)
    };

    // 2. Prepare tools
    let tools = Tools::new(&command)?;

    // 3. Resolve device path and create image file if needed
    let (storage_device_path, _image_loop) = resolve_device_path_and_image(&command)?;
    let mut storage_device = StorageDevice::from_path(
        &storage_device_path,
        command.allow_non_removable,
        command.dryrun,
    )?;

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
    let root_fs_type = command.filesystem;

    // --- NEW: Handle BTRFS subvolume setup ---
    if root_fs_type == FilesystemTypeArg::Btrfs {
        setup_btrfs_subvolumes(
            root_block_device,
            &tools.mkbtrfs,
            &tools.btrfs,
            command.dryrun,
        )?;
    } else {
        Filesystem::format(root_block_device, root_fs_type, &tools.mkext4)?;
    }
    // --- END NEW ---

    let boot_filesystem = boot_partition
        .as_ref()
        .map(|p| Filesystem::from_partition(p, FilesystemTypeArg::Vfat));
    let root_filesystem = Filesystem::from_partition(root_block_device, root_fs_type);

    // 6. Bootstrap system
    // The `bootstrap_system` function now implicitly uses the new smart `mount` tool
    let (mount_point, mount_stack) = bootstrap_system(
        &command,
        &tools,
        &boot_filesystem,
        &root_filesystem,
        &presets,
    )?;

    // 7. Copy baked sources into the image
    bake_sources_into_image(&tools, mount_point.path(), &presets_paths, &command)?;

    // 8. Apply customizations (AUR, presets)
    apply_customizations(&command, &tools.arch_chroot, &presets, mount_point.path())?;

    // 9. Install Omarchy if requested
    if command.system == SystemVariant::Omarchy {
        install_omarchy(&tools, mount_point.path(), &command)?;
    }

    // 10. Finalize installation (bootloader, services)
    finalize_installation(
        &command,
        &tools,
        &storage_device,
        &mount_point,
        encrypted_root.as_ref(),
        &root_partition_base,
    )?;

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
    temp_mount_stack.mount_single(device.path(), temp_mount.path(), Some("noatime"))?;

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

fn adjust_command_for_system(command: &mut CreateCommand) {
    if command.system == SystemVariant::Omarchy {
        info!("System variant 'Omarchy' selected. Overriding filesystem to BTRFS.");
        command.filesystem = FilesystemTypeArg::Btrfs;
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
    let boot_size_mb = command
        .boot_size
        .map_or(300, |b| (b.as_u128() / 1_048_576) as u32);
    // ... boot size validation ...

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
        Filesystem::format(bp, FilesystemTypeArg::Vfat, &tools.mkfat)?;
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
        packages.extend(settings.graphics_packages.iter().cloned());
        packages.extend(settings.font_packages.iter().cloned());
    }

    // --- NEW: Add packages based on System Variant (Omarchy) ---
    if command.system == SystemVariant::Omarchy {
        info!("Adding Omarchy specific packages (PipeWire, Bluetooth)...");
        packages.extend(
            [
                "wget",
                "pipewire",
                "pipewire-alsa",
                "pipewire-jack",
                "pipewire-pulse",
                "gst-plugin-pipewire",
                "libpulse",
                "wireplumber",
                "bluez",
                "bluez-utils",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    }

    // --- NEW: Add packages based on Filesystem Choice ---
    if command.filesystem == FilesystemTypeArg::Btrfs {
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
        let dest = baked_sources_dir.join(format!("preset_{}", i));
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
) -> anyhow::Result<()> {
    info!("Installing Omarchy...");
    // The repo is already baked into /usr/share/omarchy by bake_sources_into_image
    let omarchy_install_script = Path::new("/usr/share/omarchy/install.sh");

    if !command.dryrun
        && !mount_path
            .join(omarchy_install_script.strip_prefix("/").unwrap())
            .exists()
    {
        return Err(anyhow!(
            "Could not find baked Omarchy install script. This should not happen."
        ));
    }

    info!("Running Omarchy install script. This will be interactive.");
    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .args(["bash", omarchy_install_script.to_str().unwrap()])
        .run(command.dryrun)?;

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
        sources: sources.drain(..).collect(),
    };

    let manifest_path = mount_point.path().join("usr/share/alma/manifest.json");
    if !command.dryrun {
        let json = serde_json::to_string_pretty(&manifest)?;
        fs::write(manifest_path, json)?;
    }
    Ok(())
}

// ... other helper functions from main.rs like apply_customizations, finalize_installation, interactive_chroot_and_cleanup go here ...
// They are largely unchanged but should be moved into this file.

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
    arch_chroot
        .execute()
        .arg(mount_point.path())
        .args(["bash", "-c"])
        .arg(format!(
            "grub-install --target=i386-pc --boot-directory /boot {0} && \
             grub-install --target=x86_64-efi --efi-directory /boot --boot-directory /boot --removable {0} && \
             grub-mkconfig -o /boot/grub/grub.cfg",
            disk_path.display()
        ))
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
    use std::os::unix::fs::PermissionsExt;

    let mut script_file = tempfile::NamedTempFile::new_in(mount_path)
        .context("Failed creating temporary setup script")?;

    script_file
        .write_all(script_text.as_bytes())
        .and_then(|_| script_file.as_file_mut().metadata())
        .and_then(|metadata| {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(script_file.path(), permissions)
        })
        .context("Failed setting up temporary script")?;

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
        .run(dryrun)
        .with_context(|| format!("Failed running setup script:\n{}", script_text))?;

    Ok(())
}
