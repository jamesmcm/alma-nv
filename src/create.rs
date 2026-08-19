use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use byte_unit::Byte;
use console::style;
use dialoguer::Confirm;
use dialoguer::{Select, theme::ColorfulTheme};
use log::{debug, info, warn};
use nix::mount::MsFlags;

use crate::args::{
    CreateCommand, Manifest, OmarchyProfile, RootFilesystemType, Source, SystemVariant,
};
use crate::constants::{self, OMARCHY_MAPPER_NAME};
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

const PACSTRAP_MAX_ATTEMPTS: usize = 5;

fn fix_fstab(fstab: &str) -> String {
    fstab
        .lines()
        .filter(|line| !line.contains("swap") && !line.starts_with('#'))
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Prompts for the LUKS passphrase (with confirmation). Only used for
/// encrypted deferred-provisioning installs, where ALMA must reuse the
/// passphrase for the format/open and stage it for Quattro's first-boot
/// auto-unlock and re-key.
fn prompt_luks_passphrase() -> anyhow::Result<Vec<u8>> {
    let passphrase = dialoguer::Password::with_theme(&ColorfulTheme::default())
        .with_prompt("Enter LUKS encryption passphrase (this will be replaced at first boot)")
        .with_confirmation("Confirm passphrase", "Passphrases do not match.")
        .interact()?;
    Ok(passphrase.into_bytes())
}

/// Reads a newline-separated package manifest shipped inside the installed
/// `omarchy` package. Returns an empty set if the manifest is missing (e.g. in
/// dry-run mode).
fn read_omarchy_manifest(mount_path: &Path, manifest: &str) -> HashSet<String> {
    let path = mount_path.join(manifest);
    let mut packages = HashSet::new();
    if let Ok(content) = fs::read_to_string(&path) {
        for line in content.lines() {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                packages.insert(line.to_string());
            }
        }
    }
    packages
}

/// The bootstrap config gets a temporary hook directory so host-side Limine
/// deployment hooks cannot touch the loop-backed image. The target config
/// keeps the repository and NoExtract settings, but not that build-only path.
fn configure_omarchy_pacman_conf(
    base_conf: &Path,
    omarchy_conf_dir: &Path,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let content = fs::read_to_string(base_conf).context("Failed to read pacman.conf")?;
    let content = configure_omarchy_repo(&content);
    let stable_mirrorlist = omarchy_conf_dir.join("mirrorlist-stable");
    fs::write(&stable_mirrorlist, constants::OMARCHY_STABLE_MIRRORLIST)
        .context("Failed to write Omarchy stable mirrorlist")?;

    // Limine hooks are not useful during an offline image build. In
    // particular, a host hook can see the target ESP as /dev/loop0p1 and try
    // to register it in host firmware. Keep such hooks out of the image too,
    // so later pacman transactions do not repeat the same mistake.
    let content = add_omarchy_pacman_options(&content);
    let target_conf = omarchy_conf_dir.join("pacman-omarchy.conf");
    fs::write(&target_conf, &content).context("Failed to write Omarchy pacman.conf")?;

    let hook_dir = omarchy_conf_dir.join("bootstrap-hooks");
    neutralize_limine_hooks(&hook_dir)?;
    let bootstrap_conf = omarchy_conf_dir.join("pacman-omarchy-bootstrap.conf");
    let bootstrap_content = add_pacman_options(
        &content,
        &["DisableDownloadTimeout", "ParallelDownloads = 1"],
    );
    let bootstrap_content = format!(
        "{}\nHookDir = {}\n",
        bootstrap_content.replace(
            "Include = /etc/pacman.d/mirrorlist",
            &format!("Include = {}", stable_mirrorlist.display()),
        ),
        hook_dir.display()
    );
    fs::write(&bootstrap_conf, bootstrap_content)
        .context("Failed to write Omarchy bootstrap pacman.conf")?;

    Ok((bootstrap_conf, target_conf))
}

/// Replaces any host-defined Omarchy repository section. A host can be on a
/// different channel or package snapshot (for example stable-mirror), and
/// carrying that section into an image build can make pacman request package
/// versions that the selected mirror does not serve.
fn configure_omarchy_repo(content: &str) -> String {
    let header = format!("[{}]", constants::OMARCHY_DEFAULT_REPO_NAME);
    let server = format!(
        "Server = {}/stable/$arch",
        constants::OMARCHY_DEFAULT_REPO_URL
    );
    let canonical = [
        header.as_str(),
        server.as_str(),
        "SigLevel = Optional TrustAll",
    ];
    let mut lines = Vec::new();
    let mut in_omarchy = false;
    let mut inserted = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_omarchy = trimmed == header;
            if in_omarchy {
                if !inserted {
                    lines.extend(canonical.iter().copied());
                    inserted = true;
                }
                continue;
            }
        }
        if !in_omarchy {
            lines.push(line);
        }
    }

    if !inserted {
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
            lines.push("");
        }
        lines.extend(canonical.iter().copied());
    }

    let mut result = lines.join("\n");
    result.push('\n');
    result
}

/// Adds image-build pacman options inside `[options]`, rather than appending
/// them after a repository section where pacman would reject them.
fn add_omarchy_pacman_options(content: &str) -> String {
    add_pacman_options(
        content,
        &[
            "NoExtract = usr/share/libalpm/hooks/*limine*",
            "NoExtract = etc/pacman.d/hooks/*limine*",
        ],
    )
}

fn add_pacman_options(content: &str, options: &[&str]) -> String {
    let mut lines = Vec::new();
    let mut in_options = false;
    let mut inserted = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_options && !inserted {
                lines.extend(options.iter().copied());
                inserted = true;
            }
            in_options = trimmed == "[options]";
        }
        lines.push(line);
    }

    if in_options && !inserted {
        lines.extend(options.iter().copied());
    }

    let mut result = lines.join("\n");
    result.push('\n');
    result
}

fn run_pacstrap_with_retries(
    pacstrap: &Tool,
    pacman_conf: &Path,
    mount_path: &Path,
    use_host_cache: bool,
    packages: &[String],
    dryrun: bool,
) -> anyhow::Result<()> {
    let attempts = if dryrun { 1 } else { PACSTRAP_MAX_ATTEMPTS };

    for attempt in 1..=attempts {
        let mut command = pacstrap.execute();
        command.arg("-C").arg(pacman_conf);
        if use_host_cache {
            command.arg("-c");
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

/// Overrides host Limine hooks with equivalent no-op hooks in a later hook
/// directory. Pacman always loads its system hook directory, so merely setting
/// HookDir to an empty directory is not enough to suppress a host hook.
fn neutralize_limine_hooks(hook_dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(hook_dir)?;

    for source_dir in [
        Path::new("/usr/share/libalpm/hooks"),
        Path::new("/etc/pacman.d/hooks"),
    ] {
        let Ok(entries) = fs::read_dir(source_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let source = entry.path();
            if source.extension().and_then(|ext| ext.to_str()) != Some("hook") {
                continue;
            }
            let Ok(original) = fs::read_to_string(&source) else {
                continue;
            };
            if !original.to_ascii_lowercase().contains("limine") {
                continue;
            }

            let overridden = original
                .lines()
                .map(|line| {
                    if line.trim_start().starts_with("Exec") {
                        "Exec = /usr/bin/true".to_string()
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let filename = source
                .file_name()
                .ok_or_else(|| anyhow!("Invalid pacman hook path: {}", source.display()))?;
            fs::write(hook_dir.join(filename), format!("{overridden}\n"))?;
        }
    }

    Ok(())
}

/// Installs the non-NVRAM Limine defaults before pacstrap. This prevents a
/// package hook from trying to discover a physical disk while the ESP is
/// backed by a loop partition.
fn prepare_offline_limine(mount_path: &Path, dryrun: bool) -> anyhow::Result<()> {
    let path = mount_path.join("etc/default/limine");
    let content = "ESP_PATH=\"/boot\"\nENABLE_LIMINE_FALLBACK=yes\nSKIP_UEFI=yes\n";
    if dryrun {
        println!("write {}\n{content}", path.display());
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, content).context("Failed to prepare offline Limine defaults")?;
    }
    Ok(())
}

/// Installs Omarchy's stable Arch mirrorlist into the target. Omarchy packages
/// are built against this snapshot, not the moving mirrorlist of the build
/// host.
fn write_omarchy_mirrorlist(mount_path: &Path, dryrun: bool) -> anyhow::Result<()> {
    let path = mount_path.join("etc/pacman.d/mirrorlist");
    if dryrun {
        println!(
            "write {}\n{}",
            path.display(),
            constants::OMARCHY_STABLE_MIRRORLIST
        );
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, constants::OMARCHY_STABLE_MIRRORLIST)
            .context("Failed to write Omarchy stable mirrorlist to target")?;
    }
    Ok(())
}

pub fn create(mut command: CreateCommand) -> anyhow::Result<()> {
    // --- Initial Command Validation & Adjustments ---
    validate_command(&command)?;
    adjust_command_for_system(&mut command)?;
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

    // 4. Safety checks and partitioning.
    // For an encrypted deferred-provisioning install we capture the LUKS
    // passphrase up front so it can be piped to cryptsetup (format + open) and
    // staged for Quattro's first-boot auto-unlock/re-key. Otherwise cryptsetup
    // prompts interactively on the TTY as before.
    let luks_passphrase: Option<Vec<u8>> = if command.encrypted_root && command.defer_provisioning {
        Some(prompt_luks_passphrase()?)
    } else {
        None
    };

    confirm_and_wipe_device(&mut storage_device, &command)?;
    let (boot_partition, root_partition_base) = partition_and_format(
        &command,
        &tools,
        &storage_device,
        luks_passphrase.as_deref(),
    )?;

    // 5. Open encrypted container if requested.
    // For Omarchy we use the mapper name that Quattro itself assumes
    // (/dev/mapper/omarchy_root) so the generated boot configuration matches
    // upstream expectations. For a plain Arch install we keep "alma_root".
    let mapper_name = if command.system == SystemVariant::Omarchy {
        OMARCHY_MAPPER_NAME
    } else {
        "alma_root"
    };
    let encrypted_root = if command.encrypted_root {
        Some(EncryptedDevice::open(
            tools.cryptsetup.as_ref().unwrap(),
            &root_partition_base,
            mapper_name.into(),
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
    apply_customizations(&command, &tools.arch_chroot, &presets, mount_point.path())?;

    // 9. Finalize installation (bootloader, services) / Omarchy system setup
    if command.system == SystemVariant::Omarchy {
        let username = user_settings.as_ref().map(|s| s.username.as_str());
        finalize_omarchy_install(
            &command,
            &tools,
            &mount_point,
            boot_partition.as_ref(),
            encrypted_root.as_ref(),
            &root_partition_base,
            username,
            luks_passphrase.as_deref(),
        )?;
    } else {
        finalize_installation(
            &command,
            &tools,
            &storage_device,
            &mount_point,
            encrypted_root.as_ref(),
            &root_partition_base,
        )?;
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
    if command.defer_provisioning && !matches!(command.system, SystemVariant::Omarchy) {
        return Err(anyhow!(
            "--defer-provisioning is only supported for --system omarchy."
        ));
    }
    if command.keep_host_hardware && !matches!(command.system, SystemVariant::Omarchy) {
        return Err(anyhow!(
            "--keep-host-hardware is only supported for --system omarchy."
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
    luks_passphrase: Option<&[u8]>,
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

    let is_omarchy = command.system == SystemVariant::Omarchy;
    if is_omarchy {
        prepare_offline_limine(mount_point.path(), command.dryrun)?;
    }

    // The base package set differs for Omarchy: Quattro uses Limine rather than
    // GRUB, so we avoid pulling in the GRUB bootloader stack.
    let mut packages: HashSet<String> = if is_omarchy {
        [
            "base",
            "linux",
            "linux-firmware",
            "intel-ucode",
            "amd-ucode",
            "networkmanager",
            "rsync",
            "git",
            "base-devel",
            "limine",
            "btrfs-progs",
        ]
        .iter()
        .map(|s| String::from(*s))
        .collect()
    } else {
        constants::BASE_PACKAGES
            .iter()
            .map(|s| String::from(*s))
            .collect()
    };

    // Add interactive packages if applicable
    if let Some(settings) = user_settings {
        info!("Adding packages selected during interactive setup...");
        packages.extend(settings.graphics_packages.iter().cloned());
        packages.extend(settings.font_packages.iter().cloned());
    }

    if is_omarchy {
        info!("Adding Omarchy packages and portable hardware profile...");
        packages.extend(constants::OMARCHY_PACKAGES.iter().map(|s| s.to_string()));
        packages.extend(
            constants::OMARCHY_PORTABLE_PACKAGES
                .iter()
                .map(|s| s.to_string()),
        );

        // The `portable` profile installs a curated Omarchy core desktop set
        // (rather than the full official workstation manifest).
        if command.profile == OmarchyProfile::Portable {
            info!("Adding curated Omarchy core packages (portable profile)...");
            packages.extend(
                constants::OMARCHY_CORE_PACKAGES
                    .iter()
                    .map(|s| s.to_string()),
            );
            info!("Adding portable Omarchy development tools...");
            packages.extend(
                constants::OMARCHY_PORTABLE_DEV_PACKAGES
                    .iter()
                    .map(|s| s.to_string()),
            );
        }
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

    // For Omarchy we configure the Omarchy package repository in a dedicated
    // pacman.conf that is used for pacstrap (and copied into the image).
    let omarchy_conf_holder;
    let omarchy_target_conf;
    let pacman_conf_path: PathBuf = if is_omarchy {
        omarchy_conf_holder = tempfile::tempdir().context("Error creating a temp dir")?;
        let (bootstrap_conf, target_conf) =
            configure_omarchy_pacman_conf(&pacman_conf_path, omarchy_conf_holder.path())?;
        omarchy_target_conf = target_conf;
        bootstrap_conf
    } else {
        omarchy_target_conf = pacman_conf_path.clone();
        pacman_conf_path
    };

    info!("Bootstrapping system");
    // The package set now contains all conditional packages and command-line
    // additions. A failed download can leave the other archives in the cache,
    // so retrying the transaction avoids restarting the entire install.
    packages.extend(command.extra_packages.iter().cloned());
    let packages = packages.into_iter().collect::<Vec<_>>();
    run_pacstrap_with_retries(
        &tools.pacstrap,
        &pacman_conf_path,
        mount_point.path(),
        !is_omarchy,
        &packages,
        command.dryrun,
    )
    .context("Pacstrap error")?;

    // For Omarchy, the first pacstrap installs the `omarchy` package which
    // ships the package manifests for the full desktop. The `standard` profile
    // reads and installs the complete set so ALMA tracks upstream's package
    // list automatically. The `portable` profile stops at the curated core set
    // already installed above.
    if is_omarchy && command.profile == OmarchyProfile::Standard {
        let mut manifest_packages: HashSet<String> = HashSet::new();
        manifest_packages.extend(read_omarchy_manifest(
            mount_point.path(),
            constants::OMARCHY_BASE_MANIFEST,
        ));
        manifest_packages.extend(read_omarchy_manifest(
            mount_point.path(),
            constants::OMARCHY_OTHER_MANIFEST,
        ));

        if !manifest_packages.is_empty() {
            info!(
                "Installing {} packages from Omarchy manifests (standard profile)...",
                manifest_packages.len()
            );
            let manifest_packages = manifest_packages.into_iter().collect::<Vec<_>>();
            run_pacstrap_with_retries(
                &tools.pacstrap,
                &pacman_conf_path,
                mount_point.path(),
                false,
                &manifest_packages,
                command.dryrun,
            )
            .context("Failed to install Omarchy manifest packages")?;
        }
    }

    if !command.dryrun {
        fs::copy(
            &omarchy_target_conf,
            mount_point.path().join("etc/pacman.conf"),
        )
        .context("Failed copying pacman.conf")?;
    }
    if is_omarchy {
        write_omarchy_mirrorlist(mount_point.path(), command.dryrun)?;
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

/// Holds the device paths and UUIDs needed to configure an Omarchy 4
/// installation's storage and boot metadata. The three UUIDs are used
/// repeatedly and mixing them up produces subtle boot bugs, so they are kept
/// together.
struct OmarchyStorage {
    encrypted: bool,
    esp_device: PathBuf,
    root_partition: PathBuf,
    root_mapper: PathBuf,
    esp_uuid: String,
    luks_uuid: String,
    btrfs_uuid: String,
}

/// Queries a block device for its filesystem UUID via blkid.
fn blkid_uuid(blkid: &Tool, device: &Path, dryrun: bool) -> anyhow::Result<String> {
    let out = blkid
        .execute()
        .arg(device)
        .args(["-o", "value", "-s", "UUID"])
        .run_text_output(dryrun)
        .context("Failed to run blkid")?;
    Ok(out.trim().to_string())
}

/// Collects the device paths and UUIDs for an Omarchy installation from the
/// (optionally encrypted) root, its opened mapper, and the ESP.
fn collect_omarchy_storage(
    blkid: &Tool,
    boot_partition: Option<&Partition>,
    root_partition_base: &Partition,
    encrypted: bool,
    dryrun: bool,
) -> anyhow::Result<OmarchyStorage> {
    let root_mapper = PathBuf::from("/dev/mapper").join(OMARCHY_MAPPER_NAME);

    let esp_device = boot_partition
        .map(|p| p.path().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/boot"));

    let esp_uuid = blkid_uuid(blkid, &esp_device, dryrun).unwrap_or_default();
    // For an encrypted install the LUKS UUID belongs to the (still-closed)
    // root partition; for an unencrypted install there is no LUKS volume.
    let luks_uuid = if encrypted {
        blkid_uuid(blkid, root_partition_base.path(), dryrun)?
    } else {
        String::new()
    };
    // The Btrfs filesystem lives on the opened mapper for encrypted installs,
    // otherwise directly on the root partition.
    let btrfs_device = if encrypted {
        &root_mapper
    } else {
        root_partition_base.path()
    };
    let btrfs_uuid = blkid_uuid(blkid, btrfs_device, dryrun).unwrap_or_default();

    Ok(OmarchyStorage {
        encrypted,
        esp_device,
        root_partition: root_partition_base.path().to_path_buf(),
        root_mapper,
        esp_uuid,
        luks_uuid,
        btrfs_uuid,
    })
}

/// Writes the storage and boot metadata that Quattro expects for an already
/// partitioned/mounted target: an explicit fstab, crypttab.initramfs, the
/// kernel cmdline, and the Limine defaults.
fn write_omarchy_storage_config(
    mount_path: &Path,
    storage: &OmarchyStorage,
    dryrun: bool,
) -> anyhow::Result<()> {
    // /etc/fstab - mirror Quattro's pre-mounted layout exactly.
    let fstab = format!(
        "UUID={} /                     btrfs noatime,compress=zstd,subvol=@     0 0\n\
         UUID={} /home                 btrfs noatime,compress=zstd,subvol=@home 0 0\n\
         UUID={} /var/log              btrfs noatime,compress=zstd,subvol=@log  0 0\n\
         UUID={} /var/cache/pacman/pkg btrfs noatime,compress=zstd,subvol=@pkg  0 0\n\
         UUID={} /boot                 vfat umask=0077 0 2\n",
        storage.btrfs_uuid,
        storage.btrfs_uuid,
        storage.btrfs_uuid,
        storage.btrfs_uuid,
        storage.esp_uuid,
    );

    // /etc/crypttab.initramfs - used by the systemd initramfs to unlock LUKS.
    let mut crypttab = String::new();
    if storage.encrypted {
        crypttab = format!(
            "{} UUID={} none luks,discard\n",
            OMARCHY_MAPPER_NAME, storage.luks_uuid
        );
    }

    // /etc/kernel/cmdline - the command line embedded in the UKI by Limine.
    // This mirrors Quattro's `_build_pre_mounted_cmdline`.
    let cmdline = if storage.encrypted {
        format!(
            "cryptdevice=UUID={}:{} root=/dev/mapper/{} zswap.enabled=0 rootflags=subvol=@ rw rootfstype=btrfs",
            storage.luks_uuid, OMARCHY_MAPPER_NAME, OMARCHY_MAPPER_NAME
        )
    } else {
        format!(
            "root=UUID={} zswap.enabled=0 rootflags=subvol=@ rw rootfstype=btrfs",
            storage.btrfs_uuid
        )
    };

    if !dryrun {
        fs::write(mount_path.join("etc/fstab"), fstab).context("Failed to write fstab")?;
        if storage.encrypted {
            fs::write(mount_path.join("etc/crypttab.initramfs"), &crypttab)
                .context("Failed to write crypttab.initramfs")?;
        }
        fs::write(mount_path.join("etc/kernel/cmdline"), &cmdline)
            .context("Failed to write /etc/kernel/cmdline")?;
    } else {
        println!("write /etc/fstab\n{crypttab}write /etc/kernel/cmdline\n{cmdline}");
    }

    // /etc/default/limine - from the installed Omarchy template, with the
    // command line and removable-media fallback filled in. limine-entry-tool
    // gives /etc/default/limine the highest priority, so this overrides the
    // drop-ins shipped by omarchy-settings.
    let default_limine = mount_path.join("etc/default/limine");
    let template_path = mount_path.join(constants::OMARCHY_LIMINE_DEFAULTS);
    let mut limine_content = if let Ok(template) = fs::read_to_string(&template_path) {
        template
    } else {
        warn!(
            "Omarchy Limine default template not found at {}; using fallback",
            template_path.display()
        );
        String::from("ESP_PATH=\"/boot\"\n")
    };

    limine_content = limine_content.replace("@@CMDLINE@@", &cmdline);
    limine_content = replace_limine_option(&limine_content, "ESP_PATH", "\"/boot\"");
    limine_content = replace_limine_option(&limine_content, "ENABLE_LIMINE_FALLBACK", "yes");
    limine_content = replace_limine_option(&limine_content, "SKIP_UEFI", "yes");

    if !dryrun {
        if let Some(parent) = default_limine.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&default_limine, limine_content)
            .context("Failed to write /etc/default/limine")?;
    } else {
        println!("write /etc/default/limine\n{limine_content}");
    }

    Ok(())
}

/// Replaces or adds a `KEY=value` option line in a Limine defaults file.
fn replace_limine_option(content: &str, key: &str, value: &str) -> String {
    let line = format!("{key}={value}");
    let mut found = false;
    let mut out = String::new();
    for orig in content.lines() {
        if orig.starts_with(&format!("{key}=")) {
            out.push_str(&line);
            out.push('\n');
            found = true;
        } else {
            out.push_str(orig);
            out.push('\n');
        }
    }
    if !found {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Reasserts the image-build Limine mode after Omarchy's system setup has run.
fn enforce_offline_limine_settings(mount_path: &Path, dryrun: bool) -> anyhow::Result<()> {
    let path = mount_path.join("etc/default/limine");
    if dryrun {
        println!(
            "set ENABLE_LIMINE_FALLBACK=yes and SKIP_UEFI=yes in {}",
            path.display()
        );
        return Ok(());
    }

    let content = fs::read_to_string(&path).unwrap_or_default();
    let content = replace_limine_option(
        &replace_limine_option(&content, "ENABLE_LIMINE_FALLBACK", "yes"),
        "SKIP_UEFI",
        "yes",
    );
    fs::write(path, content).context("Failed to enforce offline Limine settings")?;
    Ok(())
}

/// Runs the root-owned Quattro system configuration inside the target.
/// `username` is `None` for a deferred-provisioning install (no user yet).
fn apply_omarchy_system(
    tools: &Tools,
    mount_path: &Path,
    username: Option<&str>,
    defer_provisioning: bool,
    dryrun: bool,
) -> anyhow::Result<()> {
    info!("Running omarchy-apply-system (Quattro root-owned system setup)...");
    let env_args = [
        "env",
        "OMARCHY_PATH=/usr/share/omarchy",
        "OMARCHY_INSTALL=/usr/share/omarchy/install",
        "OMARCHY_INSTALL_LOG_FILE=/var/log/omarchy-install.log",
    ];
    let mut cmd = tools.arch_chroot.execute();
    cmd.arg(mount_path)
        .args(env_args)
        .arg("/usr/bin/omarchy-apply-system");

    if defer_provisioning {
        cmd.arg("--defer-provisioning");
    } else {
        let user = username
            .ok_or_else(|| anyhow!("An install user is required unless deferring provisioning"))?;
        cmd.arg("--install-user").arg(user);
    }
    cmd.arg("--first-install")
        .run(dryrun)
        .context("omarchy-apply-system failed")?;
    Ok(())
}

/// Removes a staged file if it exists (no-op otherwise).
fn remove_staged_file(path: &Path, dryrun: bool) -> anyhow::Result<()> {
    if dryrun {
        println!("rm -f {}", path.display());
        return Ok(());
    }
    if path.exists() {
        info!("Removing host-specific config: {}", path.display());
        fs::remove_file(path).with_context(|| format!("Failed to remove {}", path.display()))?;
    }
    Ok(())
}

/// Removes the build host's hardware configuration from a portable target and
/// guarantees the generic `linux` kernel is installed and the default boot
/// entry. `omarchy-apply-hardware` configures for the machine ALMA runs on,
/// which is wrong for a drive meant to boot anywhere: host-forced initramfs
/// modules (NVIDIA, Apple T2, MacBook SPI, Surface) can degrade or break boot
/// on other machines, and machine-specific boot drop-ins can hide the generic
/// kernel from the boot menu.
///
/// The vendor driver packages themselves stay installed — they bind via udev
/// only when matching hardware is present.
fn sanitize_portable_hardware(
    tools: &Tools,
    mount_path: &Path,
    dryrun: bool,
) -> anyhow::Result<()> {
    info!("Sanitizing host-specific hardware configuration for portability...");

    for rel in constants::OMARCHY_PORTABLE_MKINITCPIO_DROPINS {
        remove_staged_file(&mount_path.join("etc/mkinitcpio.conf.d").join(rel), dryrun)?;
    }
    for rel in constants::OMARCHY_PORTABLE_LIMINE_DROPINS {
        remove_staged_file(
            &mount_path.join("etc/limine-entry-tool.d").join(rel),
            dryrun,
        )?;
    }
    for rel in constants::OMARCHY_PORTABLE_MODPROBE_FILES {
        remove_staged_file(&mount_path.join("etc/modprobe.d").join(rel), dryrun)?;
    }

    // `omarchy-apply-hardware` may have swapped the generic kernel for a
    // machine-specific one (e.g. linux-ptl via `pacman -Rdd linux`). Make sure
    // the generic kernel is present again so the image boots anywhere; a
    // machine-specific kernel (if any) is left installed alongside it.
    info!("Ensuring the generic linux kernel is installed...");
    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .args([
            "pacman",
            "-S",
            "--needed",
            "--noconfirm",
            "linux",
            "linux-headers",
        ])
        .run(dryrun)
        .context("Failed to ensure the generic linux kernel is installed")?;

    if !dryrun {
        verify_portable_hardware(mount_path);
    }

    Ok(())
}

/// Checks the portable sanitization held: no drop-in forces NVIDIA modules
/// into the initramfs, and the generic `linux` kernel is installed.
fn verify_portable_hardware(mount_path: &Path) {
    let mkinitcpio_dir = mount_path.join("etc/mkinitcpio.conf.d");
    if let Ok(entries) = fs::read_dir(&mkinitcpio_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(content) = fs::read_to_string(&path) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("MODULES") && trimmed.contains("nvidia") {
                        warn!(
                            "Portable validation: {} forces NVIDIA modules into the initramfs",
                            path.display()
                        );
                    }
                }
            }
        }
    }

    // The `linux` package's pacman db directory is `linux-<version>`; other
    // packages whose names merely start with "linux" (firmware, headers,
    // api-headers, linux-ptl, ...) are filtered out by requiring the
    // remainder to start with a digit (the version).
    let pacman_local = mount_path.join("var/lib/pacman/local");
    let linux_installed = fs::read_dir(&pacman_local)
        .map(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .strip_prefix("linux-")
                    .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
            })
        })
        .unwrap_or(false);
    if !linux_installed {
        warn!("Portable validation: the generic 'linux' kernel does not appear to be installed");
    }
}

/// Builds the final Limine boot configuration / UKI(s) after system setup.
fn finalize_omarchy_limine(tools: &Tools, mount_path: &Path, dryrun: bool) -> anyhow::Result<()> {
    info!("Finalizing Limine bootloader...");

    // Copy the Omarchy limine.conf to the ESP before building.
    let limine_conf_source = mount_path.join(constants::OMARCHY_LIMINE_CONF);
    if limine_conf_source.exists() {
        if !dryrun {
            fs::copy(&limine_conf_source, mount_path.join("boot/limine.conf"))
                .context("Failed to copy limine.conf to /boot")?;
        } else {
            println!("cp {} /boot/limine.conf", limine_conf_source.display());
        }
    } else if !dryrun {
        warn!(
            "Omarchy Limine conf not found at {}",
            limine_conf_source.display()
        );
    }

    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .arg("limine-update")
        .run(dryrun)
        .context("limine-update failed")?;

    // limine-update generates the UKI and menu, but its automatic installer
    // must not be responsible for the image's EFI deployment. The explicit
    // copies below are independent of the loop-device name and also guarantee
    // the removable-media fallback path exists.
    install_omarchy_limine_efi(mount_path, dryrun)?;

    // Disable Btrfs qgroup accounting so snapshots / space reporting behave
    // like upstream expects.
    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .args(["btrfs", "quota", "disable", "/"])
        .run(dryrun)
        .context("btrfs quota disable failed")?;

    Ok(())
}

/// Copies Limine's packaged UEFI binary without inspecting the mounted ESP's
/// backing device or writing an EFI NVRAM entry.
fn install_omarchy_limine_efi(mount_path: &Path, dryrun: bool) -> anyhow::Result<()> {
    let source = mount_path.join("usr/share/limine/BOOTX64.EFI");
    let limine_path = mount_path.join("boot/EFI/limine/limine_x64.efi");
    let fallback_path = mount_path.join("boot/EFI/BOOT/BOOTX64.EFI");

    if dryrun {
        println!(
            "mkdir -p {}/boot/EFI/limine {}/boot/EFI/BOOT",
            mount_path.display(),
            mount_path.display()
        );
        println!("cp {} {}", source.display(), limine_path.display());
        println!("cp {} {}", source.display(), fallback_path.display());
        return Ok(());
    }

    if !source.exists() {
        return Err(anyhow!(
            "Installed Limine EFI binary is missing: {}",
            source.display()
        ));
    }
    for destination in [&limine_path, &fallback_path] {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, destination).with_context(|| {
            format!(
                "Failed to copy Limine EFI binary to {}",
                destination.display()
            )
        })?;
    }
    Ok(())
}

/// Runs the user-owned Quattro provisioning phase for the target user.
/// Runs as the user via `arch-chroot -u`, matching the Quattro ISO.
fn provision_omarchy_user(
    tools: &Tools,
    mount_path: &Path,
    username: &str,
    dryrun: bool,
) -> anyhow::Result<()> {
    info!("Running omarchy-provision-user for '{username}'...");
    tools
        .arch_chroot
        .execute()
        .arg("-u")
        .arg(username)
        .arg(mount_path)
        .args(["env", "OMARCHY_PATH=/usr/share/omarchy"])
        .arg("/usr/bin/omarchy-provision-user")
        .arg("--force")
        .arg("--first-install")
        .run(dryrun)
        .context("omarchy-provision-user failed")?;
    Ok(())
}

/// Writes a file with the given (secret) content and mode 0600, creating any
/// missing parent directories.
fn write_secret_file(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Failed to chmod 0600 {}", path.display()))?;
    Ok(())
}

/// Stages the LUKS auto-unlock state for an encrypted deferred-provisioning
/// install, mirroring the ISO's `_stage_provisioning_luks_unlock`. The install
/// passphrase is written byte-for-byte (no trailing newline) to two 0600
/// keyfiles, and the two drop-ins make the first-boot UKI embed the keyfile and
/// unlock the root from it. `omarchy-provision-owner`'s `rekey_luks()` consumes
/// these at first boot and destroys the throwaway passphrase afterwards.
fn stage_deferred_provisioning_luks(
    mount_path: &Path,
    passphrase: &[u8],
    dryrun: bool,
) -> anyhow::Result<()> {
    info!("Staging LUKS auto-unlock keyfile for first-boot re-key...");

    let provisioning_key = mount_path.join(constants::OMARCHY_PROVISIONING_LUKS_KEY);
    let initramfs_key = mount_path.join(constants::OMARCHY_PROVISIONING_KEYFILE);
    let limine_dropin = mount_path.join(constants::OMARCHY_PROVISIONING_LIMINE_UNLOCK_DROPIN);
    let mkinitcpio_dropin = mount_path.join(constants::OMARCHY_PROVISIONING_MKINITCPIO_DROPIN);

    if dryrun {
        println!(
            "write {} <luks-passphrase> (0600)",
            provisioning_key.display()
        );
        println!("write {} <luks-passphrase> (0600)", initramfs_key.display());
        println!(
            "write {} 'KERNEL_CMDLINE[default]+=\" cryptkey=rootfs:/etc/omarchy/provisioning.key\"'",
            limine_dropin.display()
        );
        println!(
            "write {} 'FILES+=(/etc/omarchy/provisioning.key)'",
            mkinitcpio_dropin.display()
        );
        return Ok(());
    }

    // The raw passphrase, no trailing newline: `cryptsetup --key-file -`
    // treats a trailing newline as part of the key.
    write_secret_file(&provisioning_key, passphrase)?;
    write_secret_file(&initramfs_key, passphrase)?;

    fs::create_dir_all(limine_dropin.parent().unwrap_or(Path::new("/")))
        .context("Failed to create /etc/limine-entry-tool.d")?;
    fs::write(
        &limine_dropin,
        "KERNEL_CMDLINE[default]+=\" cryptkey=rootfs:/etc/omarchy/provisioning.key\"\n",
    )
    .context("Failed to write the Limine provisioning unlock drop-in")?;

    fs::create_dir_all(mkinitcpio_dropin.parent().unwrap_or(Path::new("/")))
        .context("Failed to create /etc/mkinitcpio.conf.d")?;
    fs::write(
        &mkinitcpio_dropin,
        "FILES+=(/etc/omarchy/provisioning.key)\n",
    )
    .context("Failed to write the mkinitcpio provisioning key drop-in")?;

    Ok(())
}

/// Stages the on-disk state that arms Omarchy's first-boot owner provisioning
/// (`omarchy-provision-owner.service`). This is what makes a deferred-provisioning
/// install create its user at first boot. For encrypted installs the LUKS
/// auto-unlock state is staged too, so first boot is seamless and the owner
/// provisioning re-keys the volume to the owner's new password.
fn stage_deferred_provisioning(
    mount_path: &Path,
    luks_passphrase: Option<&[u8]>,
    dryrun: bool,
) -> anyhow::Result<()> {
    let provisioning_dir = mount_path.join(constants::OMARCHY_PROVISIONING_DIR);
    let pending = mount_path.join(constants::OMARCHY_PROVISIONING_PENDING);
    let service_src = mount_path.join(constants::OMARCHY_PROVISION_OWNER_SERVICE);
    let unit_dst = mount_path.join(constants::OMARCHY_PROVISION_OWNER_UNIT);
    let wants_link = mount_path
        .join("etc/systemd/system/multi-user.target.wants")
        .join("omarchy-provision-owner.service");
    // The symlink target must be the absolute path as seen inside the target
    // chroot, not the host-side mount path.
    let unit_chroot_path = format!("/{}", constants::OMARCHY_PROVISION_OWNER_UNIT);

    if let Some(passphrase) = luks_passphrase {
        stage_deferred_provisioning_luks(mount_path, passphrase, dryrun)?;
    }

    if dryrun {
        println!("mkdir -p {}", provisioning_dir.display());
        println!("touch {}", pending.display());
        println!("cp {} {}", service_src.display(), unit_dst.display());
        println!("ln -s {unit_chroot_path} {}", wants_link.display());
        return Ok(());
    }

    fs::create_dir_all(&provisioning_dir)?;
    fs::write(&pending, "")?;
    if let Some(parent) = unit_dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if service_src.exists() {
        fs::copy(&service_src, &unit_dst)
            .context("Failed to stage omarchy-provision-owner.service")?;
    } else {
        warn!(
            "Omarchy first-boot provisioning service not found at {}",
            service_src.display()
        );
    }
    if let Some(parent) = wants_link.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&wants_link);
    std::os::unix::fs::symlink(unit_chroot_path, &wants_link)
        .context("Failed to enable omarchy-provision-owner.service")?;

    Ok(())
}

/// Configures SDDM/login state for Omarchy, mirroring the Quattro ISO's
/// `configure_login` phase.
fn configure_omarchy_login(
    mount_path: &Path,
    username: Option<&str>,
    defer_provisioning: bool,
    encrypted: bool,
    dryrun: bool,
) -> anyhow::Result<()> {
    let sddm_dir = mount_path.join("etc/sddm.conf.d");
    let login_conf =
        "[Theme]\nCurrent=omarchy\n\n[Users]\nRememberLastUser=true\nRememberLastSession=true\n"
            .to_string();

    if dryrun {
        println!("write {}/99-omarchy-login.conf", sddm_dir.display());
        return Ok(());
    }

    fs::create_dir_all(&sddm_dir)?;
    fs::write(sddm_dir.join("99-omarchy-login.conf"), login_conf)
        .context("Failed to write SDDM login config")?;

    let autologin_conf = sddm_dir.join("autologin.conf");
    if encrypted && !defer_provisioning {
        if let Some(user) = username {
            fs::write(
                &autologin_conf,
                format!("[Autologin]\nUser={user}\nSession=omarchy.desktop\n"),
            )
            .context("Failed to write SDDM autologin config")?;
        }
    } else {
        let _ = fs::remove_file(&autologin_conf);
    }

    if !defer_provisioning && let Some(user) = username {
        let state_dir = mount_path.join("var/lib/sddm");
        fs::create_dir_all(&state_dir)?;
        fs::write(
            state_dir.join("state.conf"),
            format!("[Last]\nSession=omarchy.desktop\nUser={user}\n"),
        )
        .context("Failed to write SDDM state")?;
    }

    Ok(())
}

/// Validates that the essential Omarchy boot artifacts and storage devices are
/// present.
fn validate_omarchy_install(
    mount_path: &Path,
    storage: &OmarchyStorage,
    dryrun: bool,
) -> anyhow::Result<()> {
    info!("Validating Omarchy installation...");
    let mut required = vec![
        "/boot/limine.conf",
        "/boot/EFI/BOOT/BOOTX64.EFI",
        "/etc/kernel/cmdline",
    ];
    if storage.encrypted {
        required.push("/etc/crypttab.initramfs");
    }
    let mut devices: Vec<&Path> = vec![&storage.esp_device, &storage.root_partition];
    if storage.encrypted {
        devices.push(&storage.root_mapper);
    }
    if dryrun {
        for path in required {
            println!("check: {path}");
        }
        for device in devices {
            println!("check: {}", device.display());
        }
        return Ok(());
    }
    for path in required {
        let host_path = mount_path.join(path.trim_start_matches('/'));
        if !host_path.exists() {
            warn!("Omarchy validation: expected file missing: {path}");
        }
    }
    for device in devices {
        if !device.exists() {
            warn!(
                "Omarchy validation: expected device missing: {}",
                device.display()
            );
        }
    }
    Ok(())
}

/// Orchestrates the Omarchy 4 installation. ALMA owns storage, encryption,
/// Btrfs, base bootstrap and (optionally) the user; Omarchy owns
/// system/desktop configuration, Limine/UKI generation and user provisioning.
///
/// `username` is `None` for deferred-provisioning installs (the user is created
/// at first boot by Omarchy's owner provisioning).
///
/// `luks_passphrase` is `Some` only for encrypted deferred-provisioning
/// installs, where it is staged so Quattro's first boot auto-unlocks and
/// re-keys LUKS to the owner's password.
#[allow(clippy::too_many_arguments)]
fn finalize_omarchy_install(
    command: &CreateCommand,
    tools: &Tools,
    mount_point: &TempDir,
    boot_partition: Option<&Partition>,
    encrypted_root: Option<&EncryptedDevice>,
    root_partition_base: &Partition,
    username: Option<&str>,
    luks_passphrase: Option<&[u8]>,
) -> anyhow::Result<()> {
    let mount_path = mount_point.path();
    let dryrun = command.dryrun;
    let defer = command.defer_provisioning;

    if encrypted_root.is_none() {
        warn!(
            "Omarchy is being installed without an encrypted root. For a portable installation an encrypted root is strongly recommended."
        );
    }
    if defer && command.encrypted_root && luks_passphrase.is_none() {
        return Err(anyhow!(
            "An encrypted deferred-provisioning install requires a captured LUKS passphrase to stage the first-boot auto-unlock key."
        ));
    }

    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .args(["systemctl", "enable", "NetworkManager"])
        .run(dryrun)
        .context("Failed to enable NetworkManager")?;

    if !dryrun {
        fs::write(
            mount_path.join("etc/systemd/journald.conf"),
            constants::JOURNALD_CONF,
        )
        .context("Failed to write to journald.conf")?;
    }

    let storage = collect_omarchy_storage(
        tools.blkid.as_ref().expect("No tool for blkid"),
        boot_partition,
        root_partition_base,
        command.encrypted_root,
        dryrun,
    )?;

    write_omarchy_storage_config(mount_path, &storage, dryrun)?;

    // Root-owned system configuration. For deferred-provisioning installs the
    // user does not exist yet, so pass --defer-provisioning.
    apply_omarchy_system(tools, mount_path, username, defer, dryrun)?;

    // Portable installations must not carry the build host's hardware
    // configuration (host-forced initramfs modules, machine-specific boot
    // entries). Sanitize before the final Limine/UKI build so the resulting
    // UKI reflects the sanitized state.
    if command.profile == OmarchyProfile::Portable && !command.keep_host_hardware {
        sanitize_portable_hardware(tools, mount_path, dryrun)?;
    }

    // Stage the first-boot provisioning state before the final Limine/UKI
    // build so any deferred provisioning drop-ins land in the UKI.
    if defer {
        stage_deferred_provisioning(mount_path, luks_passphrase, dryrun)?;
    }

    enforce_offline_limine_settings(mount_path, dryrun)?;

    // Final Limine/UKI build (after omarchy-apply-system has run hardware
    // setup and written its dynamic boot drop-ins).
    finalize_omarchy_limine(tools, mount_path, dryrun)?;

    // User-owned provisioning only runs when the user exists now; for deferred
    // installs it happens at first boot via omarchy-provision-owner.
    if !defer {
        let user = username
            .ok_or_else(|| anyhow!("A user is required for non-deferred Omarchy provisioning"))?;
        provision_omarchy_user(tools, mount_path, user, dryrun)?;
    }

    configure_omarchy_login(mount_path, username, defer, command.encrypted_root, dryrun)?;

    validate_omarchy_install(mount_path, &storage, dryrun)?;

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
            origin: constants::OMARCHY_DEFAULT_REPO_URL.to_string(),
            baked_path: PathBuf::from("/usr/share/omarchy"),
        });
    }

    let manifest = Manifest {
        alma_version: env!("CARGO_PKG_VERSION").to_string(),
        system_variant: command.system,
        filesystem: command.filesystem,
        encrypted_root: command.encrypted_root,
        profile: Some(command.profile),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_deferred_provisioning_luks_writes_keyfiles_and_dropins() {
        let mount = tempfile::tempdir().unwrap();
        let passphrase = b"s3cret-passphrase";

        stage_deferred_provisioning_luks(mount.path(), passphrase, false).unwrap();

        let key = mount.path().join(constants::OMARCHY_PROVISIONING_LUKS_KEY);
        assert_eq!(fs::read(&key).unwrap(), passphrase);
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let initramfs_key = mount.path().join(constants::OMARCHY_PROVISIONING_KEYFILE);
        assert_eq!(fs::read(&initramfs_key).unwrap(), passphrase);
        assert_eq!(
            fs::metadata(&initramfs_key).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let limine_dropin = mount
            .path()
            .join(constants::OMARCHY_PROVISIONING_LIMINE_UNLOCK_DROPIN);
        assert_eq!(
            fs::read_to_string(&limine_dropin).unwrap(),
            "KERNEL_CMDLINE[default]+=\" cryptkey=rootfs:/etc/omarchy/provisioning.key\"\n"
        );

        let mkinitcpio_dropin = mount
            .path()
            .join(constants::OMARCHY_PROVISIONING_MKINITCPIO_DROPIN);
        assert_eq!(
            fs::read_to_string(&mkinitcpio_dropin).unwrap(),
            "FILES+=(/etc/omarchy/provisioning.key)\n"
        );
    }

    #[test]
    fn stage_deferred_provisioning_luks_writes_no_trailing_newline() {
        let mount = tempfile::tempdir().unwrap();
        stage_deferred_provisioning_luks(mount.path(), b"pass", false).unwrap();
        let key = mount.path().join(constants::OMARCHY_PROVISIONING_LUKS_KEY);
        assert_eq!(fs::read(&key).unwrap(), b"pass");
    }

    #[test]
    fn remove_staged_file_removes_only_existing_files() {
        let mount = tempfile::tempdir().unwrap();
        let path = mount.path().join("etc/mkinitcpio.conf.d/nvidia.conf");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "MODULES+=(nvidia)\n").unwrap();

        remove_staged_file(&path, false).unwrap();
        assert!(!path.exists());

        // Removing a missing file is a no-op, not an error.
        remove_staged_file(&path, false).unwrap();
    }

    #[test]
    fn configure_omarchy_repo_replaces_host_mirror() {
        let config = "[core]\nServer = https://archlinux.org/$arch\n\n[omarchy]\nServer = https://stable-mirror.omarchy.org/$arch\n\n[extra]\n";
        let configured = configure_omarchy_repo(config);

        assert!(configured.contains("[omarchy]\nServer = https://pkgs.omarchy.org/stable/$arch"));
        assert!(!configured.contains("stable-mirror.omarchy.org"));
        assert_eq!(configured.matches("[omarchy]").count(), 1);
        assert!(configured.contains("[extra]"));
    }

    #[test]
    fn configure_omarchy_repo_appends_when_missing() {
        let configured = configure_omarchy_repo("[core]\nServer = https://archlinux.org/$arch\n");

        assert!(configured.ends_with(
            "\n[omarchy]\nServer = https://pkgs.omarchy.org/stable/$arch\nSigLevel = Optional TrustAll\n"
        ));
    }

    #[test]
    fn add_omarchy_pacman_options_keeps_noextract_in_options() {
        let configured = add_omarchy_pacman_options(
            "[options]\nArchitecture = auto\n\n[core]\nServer = https://archlinux.org/$arch\n",
        );

        assert!(configured.contains(
            "[options]\nArchitecture = auto\n\nNoExtract = usr/share/libalpm/hooks/*limine*\nNoExtract = etc/pacman.d/hooks/*limine*\n[core]"
        ));
        assert!(!configured.ends_with("NoExtract = etc/pacman.d/hooks/*limine*\n"));
    }

    #[test]
    fn bootstrap_pacman_options_relax_slow_single_mirror_downloads() {
        let configured = add_pacman_options(
            "[options]\nParallelDownloads = 5\n\n[core]\nServer = https://archlinux.org/$arch\n",
            &["DisableDownloadTimeout", "ParallelDownloads = 1"],
        );

        assert!(configured.contains(
            "[options]\nParallelDownloads = 5\n\nDisableDownloadTimeout\nParallelDownloads = 1\n[core]"
        ));
    }
}
