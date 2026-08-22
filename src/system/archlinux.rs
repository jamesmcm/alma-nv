//! Generic Arch Linux implementation of the ALMA system pipeline.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use byte_unit::Byte;
use log::info;

use crate::args::{CreateCommand, RootFilesystemType, Source};
use crate::constants;
use crate::create;
use crate::initcpio;
use crate::process::CommandExt;
use crate::system::{
    BootstrapConfig, BootstrapContext, CustomizationContext, FinalizeContext, SystemInstaller,
};
use crate::tool::Tool;

pub static INSTALLER: ArchLinux = ArchLinux;

pub struct ArchLinux;

// The generic Arch bootstrap owns the GRUB/SBAT chain. Omarchy has its own
// package bootstrap below, so this list is intentionally variant-local.
const BASE_PACKAGES: [&str; 13] = [
    "base",
    "linux",
    "linux-firmware",
    "grub",
    "efibootmgr",
    "intel-ucode",
    "amd-ucode",
    "networkmanager",
    "broadcom-wl",
    "rsync",
    "os-prober",
    "git",
    "base-devel",
];

impl SystemInstaller for ArchLinux {
    fn validate_command(&self, command: &CreateCommand) -> Result<()> {
        if command.encrypted_root && command.noconfirm {
            return Err(anyhow!(
                "Non-interactive encrypted root setup is not supported. The passphrase must be entered manually."
            ));
        }
        if command.defer_provisioning {
            return Err(anyhow!(
                "--defer-provisioning is only supported for --system omarchy."
            ));
        }
        if command.keep_host_hardware {
            return Err(anyhow!(
                "--keep-host-hardware is only supported for --system omarchy."
            ));
        }
        Ok(())
    }

    fn adjust_command(&self, _command: &mut CreateCommand) -> Result<()> {
        Ok(())
    }

    fn capture_luks_passphrase(&self, _command: &CreateCommand) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn validate_target_size(&self, _command: &CreateCommand, _total_size: Byte) -> Result<()> {
        Ok(())
    }

    fn default_boot_size_mb(&self) -> u32 {
        constants::DEFAULT_BOOT_MB
    }

    fn validate_boot_size(&self, command: &CreateCommand, boot_size_mb: u32) -> Result<()> {
        if (constants::MIN_BOOT_MB..=constants::MAX_BOOT_MB).contains(&boot_size_mb) {
            return Ok(());
        }

        log::warn!(
            "The specified boot partition size ({boot_size_mb} MiB) is outside the recommended range of {} MiB to {} MiB.",
            constants::MIN_BOOT_MB,
            constants::MAX_BOOT_MB
        );
        log::warn!(
            "A size that is too small may fail, and a size that is too large is often unnecessary."
        );

        if !command.noconfirm {
            let confirmed =
                dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
                    .with_prompt("Do you want to continue with this size?")
                    .default(false)
                    .interact()?;
            if !confirmed {
                return Err(anyhow!(
                    "User aborted operation due to boot partition size warning."
                ));
            }
        }
        Ok(())
    }

    fn mapper_name(&self) -> &'static str {
        "alma_root"
    }

    fn bootstrap_packages(
        &self,
        context: &BootstrapContext<'_>,
    ) -> std::collections::HashSet<String> {
        let mut packages = BASE_PACKAGES
            .iter()
            .map(|package| (*package).to_string())
            .collect::<std::collections::HashSet<_>>();

        if let Some(settings) = context.user_settings {
            info!("Adding packages selected during interactive setup...");
            packages.extend(settings.graphics_packages.iter().cloned());
            packages.extend(settings.font_packages.iter().cloned());
        }
        if context.portable_target {
            info!("Adding generic portable runtime packages...");
            packages.insert("zram-generator".to_string());
            packages.insert("profile-sync-daemon".to_string());
        }
        if context.command.filesystem == RootFilesystemType::Btrfs {
            packages.insert("btrfs-progs".to_string());
        }

        packages.extend(context.presets.packages.iter().cloned());
        packages.extend(context.command.extra_packages.iter().cloned());
        packages
    }

    fn configure_bootstrap(&self, base_conf: &Path) -> Result<BootstrapConfig> {
        Ok(BootstrapConfig::host(base_conf.to_path_buf()))
    }

    fn prepare_bootstrap(&self, _mount_path: &Path, _dryrun: bool) -> Result<()> {
        Ok(())
    }

    fn complete_bootstrap(
        &self,
        context: &BootstrapContext<'_>,
        config: &BootstrapConfig,
    ) -> Result<()> {
        if !context.command.dryrun {
            fs::copy(
                &config.target_pacman_conf,
                context.mount_path.join("etc/pacman.conf"),
            )
            .context("Failed copying pacman.conf")?;
        }
        Ok(())
    }

    fn apply_customizations(&self, context: &CustomizationContext<'_>) -> Result<()> {
        let mut aur_packages = vec![String::from("shim-signed")];
        aur_packages.extend(context.presets.aur_packages.clone());
        aur_packages.extend(context.command.aur_packages.clone());
        create::install_aur_packages(
            context.command,
            context.arch_chroot,
            context.mount_path,
            &aur_packages,
            false,
        )?;
        create::run_preset_scripts(context)?;
        Ok(())
    }

    fn finalize(&self, context: &FinalizeContext<'_>) -> Result<()> {
        finalize_installation(context)
    }

    fn add_manifest_sources(&self, _sources: &mut Vec<Source>) {}
}

fn finalize_installation(context: &FinalizeContext<'_>) -> Result<()> {
    let command = context.command;
    let tools = context.tools;
    let mount_path = context.mount_point.path();

    info!("Performing post installation tasks");
    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .args(["systemctl", "enable", "NetworkManager"])
        .run(command.dryrun)
        .context("Failed to enable NetworkManager")?;

    info!("Configuring journald");
    if !command.dryrun {
        fs::write(
            mount_path.join("etc/systemd/journald.conf"),
            constants::JOURNALD_CONF,
        )
        .context("Failed to write to journald.conf")?;
    }

    if context.portable_target {
        create::configure_portable_zram_policy(mount_path, command.dryrun)?;
        create::configure_portable_user_runtime(
            tools,
            mount_path,
            context.username,
            command.dryrun,
        )?;
    }

    if command.root_partition.is_none() || command.boot_partition.is_some() {
        setup_bootloader(
            context.storage_device_path,
            context.mount_point,
            &tools.arch_chroot,
            context.encrypted_root,
            context.root_partition_path,
            tools.blkid.as_ref(),
            command.dryrun,
        )?;
    }

    Ok(())
}

fn setup_bootloader(
    storage_device_path: &Path,
    mount_point: &tempfile::TempDir,
    arch_chroot: &Tool,
    encrypted_root: bool,
    root_partition_path: &Path,
    blkid: Option<&Tool>,
    dryrun: bool,
) -> Result<()> {
    info!("Starting bootloader initialisation tasks");
    info!("Generating initramfs");
    let plymouth_exists = Path::new(&mount_point.path().join("usr/bin/plymouth")).exists();
    if !dryrun {
        fs::write(
            mount_point.path().join("etc/mkinitcpio.conf"),
            initcpio::Initcpio::new(encrypted_root, plymouth_exists).to_config()?,
        )
        .context("Failed to write to mkinitcpio.conf")?;
    }
    arch_chroot
        .execute()
        .arg(mount_point.path())
        .args(["mkinitcpio", "-P"])
        .run(dryrun)
        .context("Failed to run mkinitcpio - do you have the base and linux packages installed?")?;

    if encrypted_root {
        let uuid = blkid
            .expect("No tool for blkid")
            .execute()
            .arg(root_partition_path)
            .args(["-o", "value", "-s", "UUID"])
            .run_text_output(dryrun)
            .context("Failed to run blkid")?;
        let trimmed = uuid.trim();
        if !dryrun {
            let mut grub_file = fs::OpenOptions::new()
                .append(true)
                .open(mount_point.path().join("etc/default/grub"))
                .context("Failed to create /etc/default/grub")?;
            use std::io::Write;
            write!(
                &mut grub_file,
                "GRUB_CMDLINE_LINUX=\"cryptdevice=UUID={trimmed}:luks_root\""
            )
            .context("Failed to write to /etc/default/grub")?;
        }
    }

    info!("Enabling os-prober for multi-boot detection");
    if !dryrun {
        let grub_conf_path = mount_point.path().join("etc/default/grub");
        let mut grub_conf = fs::read_to_string(&grub_conf_path)?;
        grub_conf = grub_conf.replace(
            "GRUB_DISABLE_OS_PROBER=true",
            "GRUB_DISABLE_OS_PROBER=false",
        );
        if !grub_conf.contains("GRUB_CMDLINE_LINUX") {
            grub_conf.push_str("\nGRUB_CMDLINE_LINUX=\"\"\n");
        }
        fs::write(grub_conf_path, grub_conf)?;
    }

    info!("Installing the Bootloader");
    run_grub_mkconfig_scoped(storage_device_path, mount_point, arch_chroot, dryrun)?;

    let bootloader = mount_point.path().join("boot/EFI/BOOT/BOOTX64.efi");
    if !dryrun {
        verify_sbat_section(arch_chroot, mount_point.path(), &bootloader)
            .context("GRUB EFI binary is missing the SBAT section required by shim")?;
        verify_sbat_section(
            arch_chroot,
            mount_point.path(),
            &mount_point.path().join("usr/share/shim-signed/shimx64.efi"),
        )
        .context("shim-signed EFI binary is missing the required SBAT section")?;
        verify_sbat_section(
            arch_chroot,
            mount_point.path(),
            &mount_point.path().join("usr/share/shim-signed/mmx64.efi"),
        )
        .context("shim-signed MokManager binary is missing the required SBAT section")?;

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
    }
    Ok(())
}

fn verify_sbat_section(arch_chroot: &Tool, mount_path: &Path, path: &Path) -> Result<()> {
    let target_path = path
        .strip_prefix(mount_path)
        .map(|relative| Path::new("/").join(relative))
        .with_context(|| format!("{} is outside the target root", path.display()))?;
    arch_chroot
        .execute()
        .arg(mount_path)
        .arg("objdump")
        .args(["-j", ".sbat", "-s"])
        .arg(&target_path)
        .run_text_output(false)
        .with_context(|| format!("{} has no valid .sbat section", path.display()))?;
    Ok(())
}

fn run_grub_mkconfig_scoped(
    storage_device_path: &Path,
    mount_point: &tempfile::TempDir,
    arch_chroot: &Tool,
    dryrun: bool,
) -> Result<()> {
    info!("Installing GRUB and running scoped os-prober...");
    let sbat_csv = mount_point.path().join("usr/share/grub/sbat.csv");
    if !dryrun && !sbat_csv.exists() {
        return Err(anyhow!(
            "GRUB's SBAT metadata file is missing at {}; cannot build a shim-compatible EFI binary",
            sbat_csv.display()
        ));
    }

    let disk_path = storage_device_path;
    let os_prober_path = mount_point.path().join("usr/bin/os-prober");
    let os_prober_real_path = mount_point.path().join("usr/bin/os-prober.real");
    let wrapper_script = format!(
        "#!/bin/sh\nexport OS_PROBER_DEVICES=\"{}\"\nexec /usr/bin/os-prober.real \"$@\"\n",
        disk_path.display()
    );

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

    let result = arch_chroot
        .execute()
        .arg(mount_point.path())
        .args(["bash", "-c"])
        .arg(format!(
            "grub-install --target=i386-pc --boot-directory /boot {0} && \
             grub-install --target=x86_64-efi --efi-directory /boot --boot-directory /boot --removable --sbat /usr/share/grub/sbat.csv {0} && \
             grub-mkconfig -o /boot/grub/grub.cfg",
            disk_path.display()
        ))
        .run(dryrun);

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
