use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use byte_unit::Byte;
use console::style;
use dialoguer::Confirm;
use dialoguer::{Select, theme::ColorfulTheme};
use log::{debug, info, warn};
use nix::mount::MsFlags;

use crate::args::{CreateCommand, Manifest, Source};
use crate::constants;
use crate::interactive::UserSettings;
use crate::presets::{PathWrapper, PresetsCollection, Script};
use crate::process::CommandExt;
use crate::storage::filesystem::FilesystemType;
use crate::storage::{
    self, BlockDevice, EncryptedDevice, Filesystem, LoopDevice, MountStack, StorageDevice,
    partition::Partition,
};
use crate::system::{BootstrapContext, CustomizationContext, FinalizeContext, SystemInstaller};
use crate::tool::mount;
use crate::tool::{Tool, Tools};

const PACSTRAP_MAX_ATTEMPTS: usize = 5;

fn fix_fstab(fstab: &str, portable_target: bool) -> String {
    let mut lines = fstab
        .lines()
        .filter(|line| !line.contains("swap") && !line.starts_with('#'))
        .map(str::to_string)
        .collect::<Vec<_>>();

    if portable_target {
        for line in &mut lines {
            let mut fields = line.split_whitespace();
            let _source = fields.next();
            let mount_point = fields.next();
            let filesystem = fields.next();
            let options = fields.next();

            if mount_point == Some("/boot")
                || filesystem.is_none_or(|fs| matches!(fs, "vfat" | "fat" | "efi" | "tmpfs"))
            {
                continue;
            }

            if let Some(options) = options
                && !options.split(',').any(|option| option == "commit=60")
            {
                *line = line.replacen(options, &format!("{options},commit=60"), 1);
            }
        }

        lines.push("tmpfs /var/tmp tmpfs rw,nosuid,nodev,mode=1777,size=25% 0 0".to_string());
    }

    lines.join("\n")
}

pub(crate) fn run_pacstrap_with_retries(
    pacstrap: &Tool,
    config: &crate::system::BootstrapConfig,
    mount_path: &Path,
    packages: &[String],
    dryrun: bool,
) -> anyhow::Result<()> {
    let attempts = if dryrun { 1 } else { PACSTRAP_MAX_ATTEMPTS };

    for attempt in 1..=attempts {
        let mut command = pacstrap.execute();
        command.arg("-C").arg(&config.pacman_conf);
        if config.use_host_cache {
            command.arg("-c");
        }
        if !config.use_host_mirrorlist {
            command.arg("-M");
        }
        command.arg(mount_path).args(packages);

        match command.run(dryrun) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < attempts => {
                warn!("Pacstrap attempt {attempt}/{attempts} failed: {error:#}; retrying");
                std::thread::sleep(std::time::Duration::from_secs(attempt as u64 * 2));
            }
            Err(error) => return Err(error),
        }
    }

    unreachable!("pacstrap always has at least one attempt")
}

pub fn create(mut command: CreateCommand) -> anyhow::Result<()> {
    // --- Initial Command Validation & Adjustments ---
    let system = crate::system::installer(command.system);
    system.validate_command(&command)?;
    system.adjust_command(&mut command)?;
    // We prompt for user settings unless in non-interactive mode or doing a
    // deferred-provisioning Omarchy install (where the user is created at
    // first boot by Omarchy, so there is nothing to ask here).
    let user_settings: Option<UserSettings> = if !command.noconfirm && !command.defer_provisioning {
        Some(UserSettings::prompt()?)
    } else if command.defer_provisioning {
        info!(
            "--defer-provisioning specified, skipping interactive user setup. Omarchy will provision the owner at first boot."
        );
        None
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
    // A sysfs read failure conservatively keeps the target on local-install
    // storage policies rather than enabling portable write/swap behavior.
    let portable_target = storage_device.is_removable_device().unwrap_or(false);
    if portable_target {
        info!(
            "Target {} is removable; enabling portable swap and write-wear policies",
            storage_device.path().display()
        );
    } else {
        info!(
            "Target {} is not removable; keeping local-install storage policies",
            storage_device.path().display()
        );
    }

    let total_size = command.image.unwrap_or_else(|| storage_device.size());
    system.validate_target_size(&command, total_size)?;

    // 4. Safety checks and partitioning.
    // For an encrypted deferred-provisioning install we capture the LUKS
    // passphrase up front so it can be piped to cryptsetup (format + open) and
    // staged for Quattro's first-boot auto-unlock/re-key. Otherwise cryptsetup
    // prompts interactively on the TTY as before.
    let luks_passphrase = system.capture_luks_passphrase(&command)?;

    confirm_and_wipe_device(&mut storage_device, &command)?;
    let (boot_partition, root_partition_base) = partition_and_format(
        &command,
        &tools,
        &storage_device,
        luks_passphrase.as_deref(),
        system,
    )?;

    // 5. Open encrypted container if requested.
    let encrypted_root = if command.encrypted_root {
        Some(EncryptedDevice::open(
            tools.cryptsetup.as_ref().unwrap(),
            &root_partition_base,
            system.mapper_name().into(),
            luks_passphrase.as_deref(),
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
    let mount_point = tempfile::tempdir().context("Error creating a temporary directory")?;
    let bootstrap_context = BootstrapContext {
        command: &command,
        tools: &tools,
        mount_path: mount_point.path(),
        presets: &presets,
        user_settings: user_settings.as_ref(),
        portable_target,
    };
    let mount_stack = bootstrap_system(
        &bootstrap_context,
        &boot_filesystem,
        &root_filesystem,
        &mount_point,
        system,
    )?;

    // 7. Copy baked sources into the image
    bake_sources_into_image(mount_point.path(), &presets_paths, &command)?;

    if let Some(settings) = &user_settings {
        info!("Applying settings from interactive setup...");
        // In deferred-provisioning mode we skip user creation; Omarchy creates
        // the user at first boot.
        let setup_script = settings.generate_setup_script(!command.defer_provisioning)?;
        run_script_in_chroot(
            &setup_script,
            &tools.arch_chroot,
            mount_point.path(),
            command.dryrun,
        )?;
    }

    // 8. Apply customizations (AUR, presets)
    system.apply_customizations(&CustomizationContext {
        command: &command,
        arch_chroot: &tools.arch_chroot,
        presets: &presets,
        mount_path: mount_point.path(),
    })?;

    // 9. Finalize installation (bootloader, services) / Omarchy system setup
    let username = user_settings.as_ref().map(|s| s.username.as_str());
    system.finalize(&FinalizeContext {
        command: &command,
        tools: &tools,
        mount_point: &mount_point,
        storage_device_path: storage_device.path(),
        boot_partition_path: boot_partition.as_ref().map(|partition| partition.path()),
        encrypted_root: encrypted_root.is_some(),
        root_partition_path: root_partition_base.path(),
        username,
        luks_passphrase: luks_passphrase.as_deref(),
        portable_target,
    })?;

    // 11. Generate manifest
    generate_manifest(
        &command,
        &mount_point,
        &original_command_string,
        &mut manifest_sources,
        system,
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
    luks_passphrase: Option<&[u8]>,
    system: &dyn SystemInstaller,
) -> anyhow::Result<(Option<Partition<'a>>, Partition<'a>)> {
    let boot_size_mb = command
        .boot_size
        .map_or(system.default_boot_size_mb(), |b| {
            (b.as_u128() / 1_048_576) as u32
        });
    system.validate_boot_size(command, boot_size_mb)?;

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
        EncryptedDevice::prepare(
            tools.cryptsetup.as_ref().unwrap(),
            &root_partition_base,
            luks_passphrase,
        )?;
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

fn bootstrap_system<'a, 'b>(
    context: &'a BootstrapContext<'a>,
    boot_filesystem: &'a Option<Filesystem<'b>>,
    root_filesystem: &'a Filesystem<'b>,
    mount_point: &'a tempfile::TempDir,
    system: &dyn SystemInstaller,
) -> anyhow::Result<MountStack<'a>> {
    let command = context.command;
    let tools = context.tools;
    let mount_path = mount_point.path();
    let mount_stack = mount(mount_path, boot_filesystem, root_filesystem, command.dryrun)?;

    system.prepare_bootstrap(mount_path, command.dryrun)?;

    let mut packages = system.bootstrap_packages(context);
    // `sudo` is the only common bootstrap dependency: both variants use it
    // for their AUR transaction, while each system chooses the helper path.
    packages.extend(constants::AUR_DEPENDENCIES.iter().map(|s| String::from(*s)));

    let base_pacman_conf = command
        .pacman_conf
        .clone()
        .unwrap_or_else(|| "/etc/pacman.conf".into());
    let bootstrap_config = system.configure_bootstrap(&base_pacman_conf)?;

    info!("Bootstrapping system");
    // The package set now contains the bootstrap packages. A failed download
    // can leave the other archives in the cache, so retrying the transaction
    // avoids restarting the entire install.
    let packages = packages.into_iter().collect::<Vec<_>>();
    run_pacstrap_with_retries(
        &tools.pacstrap,
        &bootstrap_config,
        mount_path,
        &packages,
        command.dryrun,
    )
    .context("Pacstrap error")?;

    system.complete_bootstrap(context, &bootstrap_config)?;
    // Pacman configuration ownership is variant-specific: generic Arch
    // preserves the selected config in complete_bootstrap, while Omarchy
    // installs its generated target config before in-target transactions.

    if !system.provides_own_fstab() {
        let fstab = fix_fstab(
            &tools
                .genfstab
                .execute()
                .arg("-U")
                .arg(mount_path)
                .run_text_output(command.dryrun)
                .context("fstab error")?,
            context.portable_target,
        );

        if !command.dryrun {
            debug!("fstab:\n{fstab}");
            fs::write(mount_path.join("etc/fstab"), fstab).context("fstab error")?;
        }
    }

    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .args(["passwd", "-d", "root"])
        .run(command.dryrun)
        .context("Failed to delete the root password")?;

    info!("Setting locale");
    if !command.dryrun {
        fs::OpenOptions::new()
            .append(true)
            .open(mount_path.join("etc/locale.gen"))
            .and_then(|mut locale_gen| locale_gen.write_all(b"en_US.UTF-8 UTF-8\n"))
            .context("Failed to create locale.gen")?;
        fs::write(mount_path.join("etc/locale.conf"), "LANG=en_US.UTF-8")
            .context("Failed to write to locale.conf")?;
    }
    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .arg("locale-gen")
        .run(command.dryrun)
        .context("locale-gen failed")?;

    Ok(mount_stack)
}

/// Applies ALMA's portable write-wear policy (zram-generator and sysctl
/// defaults). Variants that own their own zram/sysctl policy (Omarchy) simply
/// do not call this.
pub(crate) fn configure_portable_zram_policy(
    mount_path: &Path,
    dryrun: bool,
) -> anyhow::Result<()> {
    let zram_path = mount_path.join("etc/systemd/zram-generator.conf.d/99-alma-portable.conf");
    let sysctl_path = mount_path.join("etc/sysctl.d/99-alma-portable.conf");
    if !dryrun {
        fs::create_dir_all(zram_path.parent().expect("zram config has a parent"))?;
        fs::create_dir_all(sysctl_path.parent().expect("sysctl config has a parent"))?;
        fs::write(&zram_path, constants::PORTABLE_ZRAM_CONFIG)
            .context("Failed to write portable zram-generator configuration")?;
        fs::write(&sysctl_path, constants::PORTABLE_SYSCTL_CONFIG)
            .context("Failed to write portable sysctl configuration")?;
    } else {
        println!(
            "write {}\n{}",
            zram_path.display(),
            constants::PORTABLE_ZRAM_CONFIG
        );
        println!(
            "write {}\n{}",
            sysctl_path.display(),
            constants::PORTABLE_SYSCTL_CONFIG
        );
    }
    Ok(())
}

/// Applies ALMA's shared removable-target user-runtime policy: logind runtime
/// directory sizing and profile-sync-daemon defaults.
pub(crate) fn configure_portable_user_runtime(
    tools: &Tools,
    mount_path: &Path,
    username: Option<&str>,
    dryrun: bool,
) -> anyhow::Result<()> {
    let logind_path = mount_path.join("etc/systemd/logind.conf.d/99-alma-portable.conf");
    let logind_config = "[Login]\nRuntimeDirectorySize=600M\n";
    let psd_skel_path = mount_path.join("etc/skel/.config/psd/psd.conf");
    if !dryrun {
        fs::create_dir_all(logind_path.parent().expect("logind config has a parent"))?;
        fs::write(&logind_path, logind_config)
            .context("Failed to write portable user-runtime configuration")?;
        fs::create_dir_all(psd_skel_path.parent().expect("psd config has a parent"))?;
        fs::write(&psd_skel_path, constants::PORTABLE_PSD_CONFIG)
            .context("Failed to write profile-sync-daemon defaults")?;
    } else {
        println!("write {}\n{}", logind_path.display(), logind_config);
        println!(
            "write {}\n{}",
            psd_skel_path.display(),
            constants::PORTABLE_PSD_CONFIG
        );
    }

    let psd_wants = mount_path.join("etc/systemd/user/default.target.wants");
    let psd_link = psd_wants.join("psd.service");
    if !dryrun {
        fs::create_dir_all(&psd_wants)?;
        if psd_link.exists() || fs::symlink_metadata(&psd_link).is_ok() {
            fs::remove_file(&psd_link).context("Failed to replace global psd unit link")?;
        }
        symlink("/usr/lib/systemd/user/psd.service", &psd_link)
            .context("Failed to enable profile-sync-daemon for users")?;
    } else {
        println!(
            "ln -s /usr/lib/systemd/user/psd.service {}",
            psd_link.display()
        );
    }

    if let Some(username) = username {
        let user_psd_path = mount_path
            .join("home")
            .join(username)
            .join(".config/psd/psd.conf");
        if !dryrun {
            if let Some(parent) = user_psd_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&user_psd_path, constants::PORTABLE_PSD_CONFIG)
                .context("Failed to write the user's profile-sync-daemon defaults")?;
        } else {
            println!(
                "write {}\n{}",
                user_psd_path.display(),
                constants::PORTABLE_PSD_CONFIG
            );
        }
        tools
            .arch_chroot
            .execute()
            .arg(mount_path)
            .args(["chown", "-R"])
            .arg(format!("{username}:{username}"))
            .arg(format!("/home/{username}/.config/psd"))
            .run(dryrun)
            .context("Failed to assign profile-sync-daemon configuration ownership")?;
    }
    Ok(())
}

fn bake_sources_into_image(
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
    Ok(())
}

fn generate_manifest(
    command: &CreateCommand,
    mount_point: &tempfile::TempDir,
    original_command: &str,
    sources: &mut Vec<Source>,
    system: &dyn SystemInstaller,
) -> anyhow::Result<()> {
    info!("Generating installation manifest...");
    system.add_manifest_sources(sources);

    let manifest = Manifest {
        alma_version: env!("CARGO_PKG_VERSION").to_string(),
        system_variant: command.system,
        filesystem: command.filesystem,
        encrypted_root: command.encrypted_root,
        defer_provisioning: command.defer_provisioning,
        keep_host_hardware: command.keep_host_hardware,
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

pub(crate) fn install_aur_packages(
    command: &CreateCommand,
    arch_chroot: &Tool,
    mount_path: &Path,
    aur_packages: &[String],
    packaged_yay: bool,
) -> anyhow::Result<()> {
    info!("Installing AUR packages");
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

        if packaged_yay {
            info!("Using Omarchy's packaged yay for AUR packages");
            arch_chroot
                .execute()
                .arg(mount_path)
                .args(["sudo", "-u", "aur"])
                .args(crate::aur::AurHelper::Yay.get_install_command())
                .args(aur_packages)
                .run(command.dryrun)
                .context("Failed to install AUR packages with Omarchy's yay")?;
        } else {
            arch_chroot
                .execute()
                .arg(mount_path)
                .args(["sudo", "-u", "aur"])
                .arg("git")
                .arg("clone")
                .arg(format!(
                    "https://aur.archlinux.org/{}.git",
                    command.aur_helper.get_package_name()
                ))
                .arg(format!("/home/aur/{}", command.aur_helper))
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
                        command.aur_helper
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
        }

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

    Ok(())
}

pub(crate) fn run_preset_scripts(context: &CustomizationContext<'_>) -> anyhow::Result<()> {
    if !context.presets.scripts.is_empty() {
        info!("Running custom scripts");
    }
    for script in &context.presets.scripts {
        run_preset_script(
            context.command,
            context.arch_chroot,
            script,
            context.mount_path,
        )?;
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

#[allow(clippy::too_many_arguments)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_fstab_adds_portable_commit_and_tmpfs_policy() {
        let fstab = "UUID=root / ext4 rw,noatime 0 1\nUUID=boot /boot vfat umask=0077 0 2\n/dev/sda3 none swap defaults 0 0\n";
        let fixed = fix_fstab(fstab, true);

        assert!(fixed.contains("UUID=root / ext4 rw,noatime,commit=60 0 1"));
        assert!(fixed.contains("UUID=boot /boot vfat umask=0077 0 2"));
        assert!(!fixed.contains("swap"));
        assert!(fixed.contains("tmpfs /var/tmp tmpfs rw,nosuid,nodev,mode=1777,size=25% 0 0"));
    }
}
