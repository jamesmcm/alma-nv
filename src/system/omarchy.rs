//! Omarchy (Quattro) implementation of the ALMA system pipeline.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::args::{CreateCommand, RootFilesystemType, Source};
use crate::constants;
use crate::create;
use crate::process::CommandExt;
use crate::system::{
    BootstrapConfig, BootstrapContext, CustomizationContext, FinalizeContext, SystemInstaller,
};
use crate::tool::Tool;
use anyhow::{Context, Result, anyhow};
use byte_unit::{Byte, Unit, UnitType};
use dialoguer::{Confirm, theme::ColorfulTheme};
use log::{info, warn};

pub static INSTALLER: Omarchy = Omarchy;

pub struct Omarchy;

fn prompt_luks_passphrase() -> Result<Vec<u8>> {
    let passphrase = dialoguer::Password::with_theme(&ColorfulTheme::default())
        .with_prompt("Enter LUKS encryption passphrase (this will be replaced at first boot)")
        .with_confirmation("Confirm passphrase", "Passphrases do not match.")
        .interact()?;
    Ok(passphrase.into_bytes())
}

const OMARCHY_DEFAULT_BOOT_MB: u32 = 512;
const OMARCHY_MIN_BOOT_MB: u32 = 512;
const OMARCHY_MIN_TOTAL_GIB: u64 = 15;
const OMARCHY_DEFAULT_REPO_URL: &str = "https://pkgs.omarchy.org";
const OMARCHY_DEFAULT_REPO_NAME: &str = "omarchy";
const OMARCHY_STABLE_MIRRORLIST: &str =
    "Server = https://stable-mirror.omarchy.org/$repo/os/$arch\n";
const OMARCHY_MAPPER_NAME: &str = "omarchy_root";
const OMARCHY_PACKAGES: [&str; 4] = [
    "omarchy",
    "omarchy-keyring",
    "omarchy-settings",
    "omarchy-nvim",
];
const OMARCHY_BASE_MANIFEST: &str = "usr/share/omarchy/install/omarchy-base.packages";
const OMARCHY_OTHER_MANIFEST: &str = "usr/share/omarchy/install/omarchy-other.packages";
const OMARCHY_LIMINE_DEFAULTS: &str = "usr/share/omarchy/default/limine/default.conf";
const OMARCHY_LIMINE_CONF: &str = "usr/share/omarchy/default/limine/limine.conf";
const OMARCHY_PROVISIONING_DIR: &str = "var/lib/omarchy/provisioning";
const OMARCHY_PROVISIONING_PENDING: &str = "var/lib/omarchy/provisioning/pending";
const OMARCHY_PROVISIONING_LUKS_KEY: &str = "var/lib/omarchy/provisioning/luks-key";
const OMARCHY_PROVISIONING_KEYFILE: &str = "etc/omarchy/provisioning.key";
const OMARCHY_PROVISIONING_LIMINE_UNLOCK_DROPIN: &str =
    "etc/limine-entry-tool.d/99-omarchy-provisioning-unlock.conf";
const OMARCHY_PROVISIONING_MKINITCPIO_DROPIN: &str =
    "etc/mkinitcpio.conf.d/99-omarchy-provisioning-key.conf";
const OMARCHY_PROVISION_OWNER_SERVICE: &str =
    "usr/share/omarchy/install/provisioning/omarchy-provision-owner.service";
const OMARCHY_PROVISION_OWNER_UNIT: &str = "etc/systemd/system/omarchy-provision-owner.service";

const OMARCHY_PORTABLE_MKINITCPIO_DROPINS: &[&str] = &[
    "nvidia.conf",
    "apple-t2.conf",
    "macbook_spi_modules.conf",
    "surface_device_modules.conf",
];
const OMARCHY_PORTABLE_LIMINE_DROPINS: &[&str] = &[
    "zz-dell-xps-panther-lake.conf",
    "dell-xps-panther-lake.conf",
    "t2-mac.conf",
    "asus-ptl-display-backlight.conf",
    "asus-expertbook-b9406-display.conf",
    "intel-panther-lake-fred.conf",
];
const OMARCHY_PORTABLE_MODPROBE_FILES: &[&str] = &["nvidia.conf"];
// Full `omarchy-base.packages` is installed separately. These are the
// portable, non-specialist additions selected from `omarchy-other.packages`.
// Keep this list conservative: packages here are installed on every Omarchy
// target, including removable media that must boot on unrelated hardware.
const OMARCHY_GENERIC_PACKAGES: [&str; 36] = [
    "linux",
    "linux-headers",
    "linux-firmware",
    "intel-ucode",
    "amd-ucode",
    "mesa",
    "lib32-mesa",
    "vulkan-intel",
    "lib32-vulkan-intel",
    "vulkan-radeon",
    "lib32-vulkan-radeon",
    "intel-media-driver",
    "libva-intel-driver",
    "libvpl",
    "vpl-gpu-rt",
    "sof-firmware",
    "pipewire",
    "pipewire-alsa",
    "pipewire-jack",
    "pipewire-pulse",
    "gst-plugin-pipewire",
    "libpulse",
    "webp-pixbuf-loader",
    "lsp-plugins-lv2",
    "dkms",
    "limine",
    "limine-mkinitcpio-hook",
    "limine-snapper-sync",
    "snapper",
    "zram-generator",
    "btrfs-progs",
    "qt6-wayland",
    "egl-wayland",
    "gtk4-layer-shell",
    "inotify-tools",
    "rsync",
];
// Broad Arch packages remain eligible even if a future Omarchy manifest moves
// them out of `omarchy-other.packages`.
const OMARCHY_GENERIC_ALWAYS_PACKAGES: &[&str] = &[
    "intel-ucode",
    "amd-ucode",
    "mesa",
    "lib32-mesa",
    "lib32-vulkan-intel",
    "lib32-vulkan-radeon",
    "zram-generator",
];
// Hardware-gated packages that a removable build must remove when the host
// configuration selected them. Generic linux/firmware are kept alongside it.
const OMARCHY_PORTABLE_SPECIALIST_PACKAGES: &[&str] = &[
    "nvidia-open-dkms",
    "nvidia-utils",
    "lib32-nvidia-utils",
    "libva-nvidia-driver",
    "nvidia-dkms",
    "nvidia-580xx-dkms",
    "nvidia-580xx-utils",
    "lib32-nvidia-580xx-utils",
    "linux-ptl",
    "linux-ptl-headers",
    "intel-ipu7-camera",
    "broadcom-wl",
    "apple-bcm-firmware",
    "apple-t2-audio-config",
    "linux-t2",
    "linux-t2-headers",
    "t2fanrd",
    "macbook12-spi-driver-dkms",
    "asusctl",
    "dell-xps-touchpad-haptics",
    "tuxedo-drivers-nocompatcheck-dkms",
    "yt6801-dkms",
    "qmk-hid",
    "vulkan-asahi",
];
const PORTABLE_SNAPSHOTS_HOOK: &str = "[Trigger]\n\
Type = Package\n\
Operation = Install\n\
Operation = Upgrade\n\
Target = omarchy\n\
Target = omarchy-settings\n\
Target = snapper\n\
\n\
[Action]\n\
Description = Reapply ALMA portable snapshot policy\n\
When = PostTransaction\n\
Exec = /usr/local/lib/alma/disable-omarchy-snapshots\n";
const PORTABLE_SNAPSHOTS_SCRIPT: &str = "#!/bin/sh\n\
set -u\n\
for unit in snapper-timeline.timer snapper-cleanup.timer limine-snapper-sync.service limine-snapper-sync.path; do\n\
    systemctl mask --now \"$unit\" >/dev/null 2>&1 || true\n\
done\n\
rm -f /etc/snapper/configs/root\n\
if command -v btrfs >/dev/null 2>&1 && [ -d /.snapshots ]; then\n\
    btrfs subvolume delete /.snapshots >/dev/null 2>&1 || true\n\
fi\n";

/// Reads a newline-separated package manifest shipped inside the installed
/// `omarchy` package. Returns an empty set if the manifest is missing (e.g. in
/// dry-run mode).
pub(crate) fn read_manifest(mount_path: &Path, manifest: &str) -> HashSet<String> {
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

/// Applies Omarchy's package conflict policy to supplemental requests. The
/// upstream manifests remain the source of truth; this table only handles
/// aliases that can otherwise replace an Omarchy-owned package in a later
/// transaction.
pub(crate) fn prioritize_packages(
    packages: impl IntoIterator<Item = String>,
    base_packages: &HashSet<String>,
) -> Vec<String> {
    // Audit preferred names against the manifests and pacman metadata:
    // `pacman -Si <package>` (Conflicts With/Provides/Replaces),
    // `pacman -Qi <installed>`, and the upstream files at
    // install/omarchy-base.packages and install/omarchy-other.packages.
    // AUR-only aliases stay here because they are not guaranteed to exist in
    // the sync databases before the AUR transaction.
    const CONFLICT_GROUPS: &[&[&str]] = &[
        &["yay", "yay-bin"],
        &["paru", "paru-bin"],
        &["mise", "mise-bin"],
        &["quickshell", "quickshell-git"],
        &["nvim", "neovim"],
        &["libvpl", "onevpl"],
        &["vpl-gpu-rt", "onevpl-intel-gpu"],
        &["pipewire-pulse", "pulseaudio"],
        &["pipewire-jack", "jack", "jack2"],
        &["btrfs-progs", "btrfs-progs-unstable"],
    ];

    let packages = packages.into_iter().collect::<Vec<_>>();
    let requested = packages.iter().cloned().collect::<HashSet<_>>();
    packages
        .into_iter()
        .filter(|package| {
            if package == "shim-signed" {
                warn!("Skipping shim-signed for Omarchy because Omarchy boots with Limine");
                return false;
            }
            if let Some(group) = CONFLICT_GROUPS
                .iter()
                .find(|group| group.contains(&package.as_str()))
            {
                let preferred = group
                    .iter()
                    .find(|candidate| base_packages.contains(**candidate))
                    .or_else(|| {
                        group
                            .iter()
                            .find(|candidate| requested.contains(**candidate))
                    });
                if let Some(preferred) = preferred
                    && *preferred != package.as_str()
                {
                    warn!(
                        "Skipping Omarchy-conflicting package '{}' in favor of '{}'",
                        package, preferred
                    );
                    return false;
                }
            }
            true
        })
        .collect()
}

/// Installs packages supplementing the complete Omarchy base manifest through
/// the target's `omarchy-pkg-add` lifecycle.
pub(crate) fn install_package_additions(
    arch_chroot: &Tool,
    mount_path: &Path,
    packages: &[String],
    dryrun: bool,
) -> Result<()> {
    if packages.is_empty() {
        return Ok(());
    }
    info!(
        "Installing {} Omarchy supplemental packages through omarchy-pkg-add...",
        packages.len()
    );
    arch_chroot
        .execute()
        .arg(mount_path)
        .arg("omarchy-pkg-add")
        .args(packages)
        .run(dryrun)
        .context("Failed to install Omarchy supplemental packages")?;
    Ok(())
}

/// Builds isolated pacman configuration for the initial Omarchy transaction.
/// The stable Omarchy mirror is selected explicitly and the host's Limine
/// hooks are neutralized while pacstrap operates on the loop-backed target.
pub(crate) fn configure_pacman_conf(
    base_conf: &Path,
    omarchy_conf_dir: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let content = fs::read_to_string(base_conf).context("Failed to read pacman.conf")?;
    let content = configure_repo(&content);
    let content = configure_official_repos(&content);
    let stable_mirrorlist = omarchy_conf_dir.join("mirrorlist-stable");
    fs::write(&stable_mirrorlist, OMARCHY_STABLE_MIRRORLIST)
        .context("Failed to write Omarchy stable mirrorlist")?;

    let content = add_pacman_options(
        &content,
        &[
            "NoExtract = usr/share/libalpm/hooks/*limine*",
            "NoExtract = etc/pacman.d/hooks/*limine*",
        ],
    );
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

fn configure_repo(content: &str) -> String {
    let header = format!("[{}]", OMARCHY_DEFAULT_REPO_NAME);
    let server = format!("Server = {}/stable/$arch", OMARCHY_DEFAULT_REPO_URL);
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

fn configure_official_repos(content: &str) -> String {
    const OFFICIAL_REPOS: [&str; 3] = ["core", "extra", "multilib"];
    const DISABLED_TESTING_REPOS: [&str; 6] = [
        "core-testing",
        "extra-testing",
        "multilib-testing",
        "community",
        "community-testing",
        "testing",
    ];
    let mut lines = Vec::new();
    let mut in_official_repo = false;
    let mut seen = HashSet::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = &trimmed[1..trimmed.len() - 1];
            if OFFICIAL_REPOS.contains(&section) {
                if seen.insert(section.to_string()) {
                    if !lines.is_empty()
                        && !lines.last().is_some_and(|line: &String| line.is_empty())
                    {
                        lines.push(String::new());
                    }
                    lines.push(format!("[{section}]"));
                    lines.push(String::from("Include = /etc/pacman.d/mirrorlist"));
                }
                in_official_repo = true;
                continue;
            }
            if DISABLED_TESTING_REPOS.contains(&section) {
                in_official_repo = true;
                continue;
            }
            in_official_repo = false;
        }
        if !in_official_repo {
            lines.push(line.to_string());
        }
    }
    for repo in OFFICIAL_REPOS {
        if seen.insert(repo.to_string()) {
            if !lines.is_empty() && !lines.last().is_some_and(|line: &String| line.is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{repo}]"));
            lines.push(String::from("Include = /etc/pacman.d/mirrorlist"));
        }
    }
    let mut result = lines.join("\n");
    result.push('\n');
    result
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

fn neutralize_limine_hooks(hook_dir: &Path) -> Result<()> {
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

pub(crate) fn prepare_offline_limine(mount_path: &Path, dryrun: bool) -> Result<()> {
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

pub(crate) fn write_mirrorlist(mount_path: &Path, dryrun: bool) -> Result<()> {
    let path = mount_path.join("etc/pacman.d/mirrorlist");
    if dryrun {
        println!("write {}\n{}", path.display(), OMARCHY_STABLE_MIRRORLIST);
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, OMARCHY_STABLE_MIRRORLIST)
            .context("Failed to write Omarchy stable mirrorlist to target")?;
    }
    Ok(())
}

struct OmarchyStorage {
    encrypted: bool,
    esp_device: PathBuf,
    root_partition: PathBuf,
    root_mapper: PathBuf,
    esp_uuid: String,
    luks_uuid: String,
    btrfs_uuid: String,
}

fn blkid_uuid(blkid: &Tool, device: &Path, dryrun: bool) -> Result<String> {
    let out = blkid
        .execute()
        .arg(device)
        .args(["-o", "value", "-s", "UUID"])
        .run_text_output(dryrun)
        .context("Failed to run blkid")?;
    Ok(out.trim().to_string())
}

fn collect_storage(
    blkid: &Tool,
    boot_partition_path: Option<&Path>,
    root_partition_path: &Path,
    encrypted: bool,
    dryrun: bool,
) -> Result<OmarchyStorage> {
    let root_mapper = PathBuf::from("/dev/mapper").join(OMARCHY_MAPPER_NAME);
    let esp_device = boot_partition_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/boot"));
    let esp_uuid = blkid_uuid(blkid, &esp_device, dryrun).unwrap_or_default();
    let luks_uuid = if encrypted {
        blkid_uuid(blkid, root_partition_path, dryrun)?
    } else {
        String::new()
    };
    let btrfs_device = if encrypted {
        &root_mapper
    } else {
        root_partition_path
    };
    let btrfs_uuid = blkid_uuid(blkid, btrfs_device, dryrun).unwrap_or_default();
    Ok(OmarchyStorage {
        encrypted,
        esp_device,
        root_partition: root_partition_path.to_path_buf(),
        root_mapper,
        esp_uuid,
        luks_uuid,
        btrfs_uuid,
    })
}

fn write_storage_config(
    mount_path: &Path,
    storage: &OmarchyStorage,
    dryrun: bool,
    portable_target: bool,
) -> Result<()> {
    let commit = if portable_target { ",commit=60" } else { "" };
    let portable_tmp = if portable_target {
        "tmpfs /var/tmp              tmpfs rw,nosuid,nodev,mode=1777,size=25% 0 0\n"
    } else {
        ""
    };
    let fstab = format!(
        "UUID={} /                     btrfs noatime,compress=zstd{},subvol=@     0 0\n\
         UUID={} /home                 btrfs noatime,compress=zstd{},subvol=@home 0 0\n\
         UUID={} /var/log              btrfs noatime,compress=zstd{},subvol=@log  0 0\n\
         UUID={} /var/cache/pacman/pkg btrfs noatime,compress=zstd{},subvol=@pkg  0 0\n\
         UUID={} /boot                 vfat umask=0077 0 2\n\
         {}",
        storage.btrfs_uuid,
        commit,
        storage.btrfs_uuid,
        commit,
        storage.btrfs_uuid,
        commit,
        storage.btrfs_uuid,
        commit,
        storage.esp_uuid,
        portable_tmp,
    );
    let crypttab = if storage.encrypted {
        format!(
            "{} UUID={} none luks,discard\n",
            OMARCHY_MAPPER_NAME, storage.luks_uuid
        )
    } else {
        String::new()
    };
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

    let default_limine = mount_path.join("etc/default/limine");
    let template_path = mount_path.join(OMARCHY_LIMINE_DEFAULTS);
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

fn replace_limine_option(content: &str, key: &str, value: &str) -> String {
    let line = format!("{key}={value}");
    let mut found = false;
    let mut out = String::new();
    for original in content.lines() {
        if original.starts_with(&format!("{key}=")) {
            out.push_str(&line);
            out.push('\n');
            found = true;
        } else {
            out.push_str(original);
            out.push('\n');
        }
    }
    if !found {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn enforce_offline_limine_settings(mount_path: &Path, dryrun: bool) -> Result<()> {
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

fn apply_system(
    tools: &crate::tool::Tools,
    mount_path: &Path,
    username: Option<&str>,
    defer_provisioning: bool,
    dryrun: bool,
) -> Result<()> {
    info!("Running omarchy-apply-system (Quattro root-owned system setup)...");
    let env_args = [
        "env",
        "OMARCHY_PATH=/usr/share/omarchy",
        "OMARCHY_INSTALL=/usr/share/omarchy/install",
        "OMARCHY_INSTALL_LOG_FILE=/var/log/omarchy-install.log",
    ];
    let mut command = tools.arch_chroot.execute();
    command
        .arg(mount_path)
        .args(env_args)
        .arg("/usr/bin/omarchy-apply-system");
    if defer_provisioning {
        command.arg("--defer-provisioning");
    } else {
        let user = username
            .ok_or_else(|| anyhow!("An install user is required unless deferring provisioning"))?;
        command.arg("--install-user").arg(user);
    }
    command
        .arg("--first-install")
        .run(dryrun)
        .context("omarchy-apply-system failed")?;
    Ok(())
}

fn remove_staged_file(path: &Path, dryrun: bool) -> Result<()> {
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

fn configure_portable_snapshots(
    tools: &crate::tool::Tools,
    mount_path: &Path,
    dryrun: bool,
) -> Result<()> {
    let units = [
        "snapper-timeline.timer",
        "snapper-cleanup.timer",
        "limine-snapper-sync.service",
        "limine-snapper-sync.path",
    ];
    let systemd_dir = mount_path.join("etc/systemd/system");
    for unit in units {
        let path = systemd_dir.join(unit);
        if !dryrun {
            fs::create_dir_all(&systemd_dir)?;
            if path.exists() || fs::symlink_metadata(&path).is_ok() {
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to replace snapshot unit {path:?}"))?;
            }
            std::os::unix::fs::symlink("/dev/null", &path)
                .with_context(|| format!("Failed to mask snapshot unit {path:?}"))?;
        } else {
            println!("ln -s /dev/null {}", path.display());
        }
    }

    let config = mount_path.join("etc/snapper/configs/root");
    if !dryrun {
        if config.exists() || fs::symlink_metadata(&config).is_ok() {
            fs::remove_file(&config).context("Failed to remove the portable Snapper config")?;
        }
    } else {
        println!("rm -f {}", config.display());
    }

    let hook = mount_path.join("etc/pacman.d/hooks/99-alma-portable-snapper.hook");
    let script = mount_path.join("usr/local/lib/alma/disable-omarchy-snapshots");
    if !dryrun {
        fs::create_dir_all(hook.parent().expect("snapshot hook has a parent"))?;
        fs::write(&hook, PORTABLE_SNAPSHOTS_HOOK)
            .context("Failed to write portable Snapper pacman hook")?;
        fs::create_dir_all(script.parent().expect("snapshot script has a parent"))?;
        fs::write(&script, PORTABLE_SNAPSHOTS_SCRIPT)
            .context("Failed to write portable Snapper policy script")?;
        fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    } else {
        println!("write {}\n{}", hook.display(), PORTABLE_SNAPSHOTS_HOOK);
        println!("write {}\n{}", script.display(), PORTABLE_SNAPSHOTS_SCRIPT);
    }

    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .arg("/usr/local/lib/alma/disable-omarchy-snapshots")
        .run(dryrun)
        .context("Failed to apply portable Snapper policy")?;
    Ok(())
}

fn sanitize_portable_hardware(
    tools: &crate::tool::Tools,
    mount_path: &Path,
    dryrun: bool,
) -> Result<()> {
    info!("Sanitizing host-specific hardware configuration for portability...");
    for relative in OMARCHY_PORTABLE_MKINITCPIO_DROPINS {
        remove_staged_file(
            &mount_path.join("etc/mkinitcpio.conf.d").join(relative),
            dryrun,
        )?;
    }
    for relative in OMARCHY_PORTABLE_LIMINE_DROPINS {
        remove_staged_file(
            &mount_path.join("etc/limine-entry-tool.d").join(relative),
            dryrun,
        )?;
    }
    for relative in OMARCHY_PORTABLE_MODPROBE_FILES {
        remove_staged_file(&mount_path.join("etc/modprobe.d").join(relative), dryrun)?;
    }

    let specialist_names = OMARCHY_PORTABLE_SPECIALIST_PACKAGES.join(" ");
    let installed = tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .args(["bash", "-c"])
        .arg(format!(
            "pacman -Qq {} 2>/dev/null || true",
            specialist_names
        ))
        .run_text_output(dryrun)
        .context("Failed to inspect host-specific Omarchy packages")?;
    let official_base = read_manifest(mount_path, OMARCHY_BASE_MANIFEST);
    let installed = installed
        .lines()
        .map(str::trim)
        .filter(|package| !package.is_empty())
        .filter(|package| !official_base.contains(*package))
        .map(String::from)
        .collect::<Vec<_>>();
    if !installed.is_empty() {
        info!(
            "Removing host-specific Omarchy packages from portable target: {}",
            installed.join(" ")
        );
        if let Err(error) = tools
            .arch_chroot
            .execute()
            .arg(mount_path)
            .args(["pacman", "-Rns", "--noconfirm"])
            .args(&installed)
            .run(dryrun)
        {
            warn!(
                "Could not remove every host-specific Omarchy package; leaving the package set intact: {error:#}"
            );
        }
    }

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

fn finalize_limine(tools: &crate::tool::Tools, mount_path: &Path, dryrun: bool) -> Result<()> {
    info!("Finalizing Limine bootloader...");
    let limine_conf_source = mount_path.join(OMARCHY_LIMINE_CONF);
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
    install_limine_efi(mount_path, dryrun)?;
    tools
        .arch_chroot
        .execute()
        .arg(mount_path)
        .args(["btrfs", "quota", "disable", "/"])
        .run(dryrun)
        .context("btrfs quota disable failed")?;
    Ok(())
}

fn install_limine_efi(mount_path: &Path, dryrun: bool) -> Result<()> {
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

fn provision_user(
    tools: &crate::tool::Tools,
    mount_path: &Path,
    username: &str,
    dryrun: bool,
) -> Result<()> {
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

fn write_secret_file(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Failed to chmod 0600 {}", path.display()))?;
    Ok(())
}

fn stage_deferred_provisioning_luks(
    mount_path: &Path,
    passphrase: &[u8],
    dryrun: bool,
) -> Result<()> {
    info!("Staging LUKS auto-unlock keyfile for first-boot re-key...");
    let provisioning_key = mount_path.join(OMARCHY_PROVISIONING_LUKS_KEY);
    let initramfs_key = mount_path.join(OMARCHY_PROVISIONING_KEYFILE);
    let limine_dropin = mount_path.join(OMARCHY_PROVISIONING_LIMINE_UNLOCK_DROPIN);
    let mkinitcpio_dropin = mount_path.join(OMARCHY_PROVISIONING_MKINITCPIO_DROPIN);
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

fn stage_deferred_provisioning(
    mount_path: &Path,
    luks_passphrase: Option<&[u8]>,
    dryrun: bool,
) -> Result<()> {
    let provisioning_dir = mount_path.join(OMARCHY_PROVISIONING_DIR);
    let pending = mount_path.join(OMARCHY_PROVISIONING_PENDING);
    let service_src = mount_path.join(OMARCHY_PROVISION_OWNER_SERVICE);
    let unit_dst = mount_path.join(OMARCHY_PROVISION_OWNER_UNIT);
    let wants_link = mount_path
        .join("etc/systemd/system/multi-user.target.wants")
        .join("omarchy-provision-owner.service");
    let unit_chroot_path = format!("/{}", OMARCHY_PROVISION_OWNER_UNIT);
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

fn configure_login(
    mount_path: &Path,
    username: Option<&str>,
    defer_provisioning: bool,
    encrypted: bool,
    dryrun: bool,
) -> Result<()> {
    let sddm_dir = mount_path.join("etc/sddm.conf.d");
    let login_conf =
        "[Theme]\nCurrent=omarchy\n\n[Users]\nRememberLastUser=true\nRememberLastSession=true\n";
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

fn validate_install(mount_path: &Path, storage: &OmarchyStorage, dryrun: bool) -> Result<()> {
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

fn finalize_install(context: &FinalizeContext<'_>) -> Result<()> {
    let command = context.command;
    let tools = context.tools;
    let mount_path = context.mount_point.path();
    let dryrun = command.dryrun;
    let defer = command.defer_provisioning;
    if !context.encrypted_root {
        warn!(
            "Omarchy is being installed without an encrypted root. For a portable installation an encrypted root is strongly recommended."
        );
    }
    if defer && command.encrypted_root && context.luks_passphrase.is_none() {
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

    let storage = collect_storage(
        tools.blkid.as_ref().expect("No tool for blkid"),
        context.boot_partition_path,
        context.root_partition_path,
        context.encrypted_root,
        dryrun,
    )?;
    write_storage_config(mount_path, &storage, dryrun, context.portable_target)?;
    apply_system(tools, mount_path, context.username, defer, dryrun)?;
    if context.portable_target {
        // Omarchy owns the zram-generator and sysctl defaults. ALMA adds only
        // the generic user-runtime/PSD policy and snapshot policy here.
        create::configure_portable_runtime(tools, mount_path, context.username, true, dryrun)?;
        configure_portable_snapshots(tools, mount_path, dryrun)?;
    }
    if context.portable_target && !command.keep_host_hardware {
        sanitize_portable_hardware(tools, mount_path, dryrun)?;
    }
    if defer {
        stage_deferred_provisioning(mount_path, context.luks_passphrase, dryrun)?;
    }
    enforce_offline_limine_settings(mount_path, dryrun)?;
    finalize_limine(tools, mount_path, dryrun)?;
    if !defer {
        let user = context
            .username
            .ok_or_else(|| anyhow!("A user is required for non-deferred Omarchy provisioning"))?;
        provision_user(tools, mount_path, user, dryrun)?;
    }
    configure_login(
        mount_path,
        context.username,
        defer,
        context.encrypted_root,
        dryrun,
    )?;
    validate_install(mount_path, &storage, dryrun)
}

impl SystemInstaller for Omarchy {
    fn validate_command(&self, command: &CreateCommand) -> Result<()> {
        if command.noconfirm {
            return Err(anyhow!(
                "Non-interactive installation (--noconfirm) is not supported for Omarchy."
            ));
        }
        Ok(())
    }

    fn adjust_command(&self, command: &mut CreateCommand) -> Result<()> {
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
        } else if !matches!(command.aur_helper, crate::aur::AurHelper::Yay) {
            warn!(
                "Omarchy supplies and prioritizes yay; ignoring the alternate AUR helper selection."
            );
            command.aur_helper = crate::aur::AurHelper::Yay;
        }
        Ok(())
    }

    fn capture_luks_passphrase(&self, command: &CreateCommand) -> Result<Option<Vec<u8>>> {
        if command.encrypted_root && command.defer_provisioning {
            Ok(Some(prompt_luks_passphrase()?))
        } else {
            Ok(None)
        }
    }

    fn validate_target_size(&self, command: &CreateCommand, total_size: Byte) -> Result<()> {
        let minimum = Byte::from_u64_with_unit(OMARCHY_MIN_TOTAL_GIB, Unit::GiB)
            .expect("Omarchy minimum size is representable")
            .as_u128();
        if total_size.as_u128() >= minimum {
            return Ok(());
        }

        warn!(
            "The selected device/image size ({}) is less than the recommended minimum of {} for Omarchy.",
            total_size.get_appropriate_unit(UnitType::Both),
            Byte::from_u128(minimum)
                .expect("Omarchy minimum size is representable")
                .get_appropriate_unit(UnitType::Both)
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
        Ok(())
    }

    fn default_boot_size_mb(&self) -> u32 {
        OMARCHY_DEFAULT_BOOT_MB
    }

    fn validate_boot_size(&self, command: &CreateCommand, boot_size_mb: u32) -> Result<()> {
        if boot_size_mb >= OMARCHY_MIN_BOOT_MB {
            return Ok(());
        }

        warn!(
            "The specified boot partition size ({} MiB) is less than the recommended minimum of {} MiB for Omarchy.",
            boot_size_mb, OMARCHY_MIN_BOOT_MB
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
        Ok(())
    }

    fn mapper_name(&self) -> &'static str {
        OMARCHY_MAPPER_NAME
    }

    fn bootstrap_packages(&self, context: &BootstrapContext<'_>) -> HashSet<String> {
        let mut packages = [
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
        .into_iter()
        .map(String::from)
        .collect::<HashSet<_>>();

        if let Some(settings) = context.user_settings {
            info!("Adding packages selected during interactive setup...");
            packages.extend(settings.graphics_packages.iter().cloned());
            packages.extend(settings.font_packages.iter().cloned());
        }
        info!("Adding Omarchy bootstrap packages...");
        packages.extend(OMARCHY_PACKAGES.iter().map(|s| s.to_string()));
        if context.command.filesystem == RootFilesystemType::Btrfs {
            packages.insert("btrfs-progs".to_string());
        }
        packages
    }

    fn configure_bootstrap(&self, base_conf: &Path) -> Result<BootstrapConfig> {
        let temp_dir =
            tempfile::tempdir().context("Error creating Omarchy pacman config directory")?;
        let (pacman_conf, target_pacman_conf) = configure_pacman_conf(base_conf, temp_dir.path())?;
        Ok(BootstrapConfig {
            pacman_conf,
            target_pacman_conf,
            use_host_cache: false,
            use_host_mirrorlist: false,
            _temp_dir: Some(temp_dir),
        })
    }

    fn prepare_bootstrap(&self, mount_path: &Path, dryrun: bool) -> Result<()> {
        prepare_offline_limine(mount_path, dryrun)
    }

    fn complete_bootstrap(
        &self,
        context: &BootstrapContext<'_>,
        config: &BootstrapConfig,
    ) -> Result<()> {
        let base_manifest = read_manifest(context.mount_path, OMARCHY_BASE_MANIFEST);
        let other_manifest = read_manifest(context.mount_path, OMARCHY_OTHER_MANIFEST);
        let manifest_packages = base_manifest.clone();
        let mut additional_packages = HashSet::new();
        for package in OMARCHY_GENERIC_PACKAGES {
            if base_manifest.contains(package) {
                continue;
            }
            if other_manifest.contains(package)
                || OMARCHY_GENERIC_ALWAYS_PACKAGES.contains(&package)
            {
                additional_packages.insert((*package).to_string());
            } else {
                warn!(
                    "Omarchy generic package '{}' is not present in either shipped manifest; skipping it",
                    package
                );
            }
        }

        let mut requested_packages = context.presets.packages.clone();
        requested_packages.extend(context.command.extra_packages.iter().cloned());
        additional_packages.extend(prioritize_packages(requested_packages, &base_manifest));

        if !manifest_packages.is_empty() {
            info!(
                "Installing {} packages from the complete Omarchy base manifest...",
                manifest_packages.len()
            );
            let manifest_packages = manifest_packages.into_iter().collect::<Vec<_>>();
            create::run_pacstrap_with_retries(
                &context.tools.pacstrap,
                &config.pacman_conf,
                context.mount_path,
                false,
                &manifest_packages,
                context.command.dryrun,
                false,
            )
            .context("Failed to install Omarchy manifest packages")?;
        }

        if context.portable_target {
            additional_packages.insert("profile-sync-daemon".to_string());
        }
        let mut additional_packages = additional_packages.into_iter().collect::<Vec<_>>();
        additional_packages.sort_unstable();
        let additional_packages = prioritize_packages(additional_packages, &base_manifest);

        if !context.command.dryrun {
            fs::copy(
                &config.target_pacman_conf,
                context.mount_path.join("etc/pacman.conf"),
            )
            .context("Failed copying pacman.conf")?;
        }
        write_mirrorlist(context.mount_path, context.command.dryrun)?;
        install_package_additions(
            &context.tools.arch_chroot,
            context.mount_path,
            &additional_packages,
            context.command.dryrun,
        )?;
        Ok(())
    }

    fn apply_customizations(&self, context: &CustomizationContext<'_>) -> Result<()> {
        let mut aur_packages = context.presets.aur_packages.clone();
        aur_packages.extend(context.command.aur_packages.clone());
        let mut official = read_manifest(context.mount_path, OMARCHY_BASE_MANIFEST);
        official.insert(String::from("yay"));
        let aur_packages = prioritize_packages(aur_packages, &official);
        create::install_aur_packages(
            context.command,
            context.arch_chroot,
            context.mount_path,
            &aur_packages,
            true,
        )?;
        create::run_preset_scripts(context)?;
        Ok(())
    }

    fn finalize(&self, context: &FinalizeContext<'_>) -> Result<()> {
        finalize_install(context)
    }

    fn add_manifest_sources(&self, sources: &mut Vec<Source>) {
        sources.push(Source {
            r#type: "system".to_string(),
            origin: OMARCHY_DEFAULT_REPO_URL.to_string(),
            baked_path: std::path::PathBuf::from("/usr/share/omarchy"),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_deferred_provisioning_luks_writes_keyfiles_and_dropins() {
        let mount = tempfile::tempdir().unwrap();
        let passphrase = b"s3cret-passphrase";

        stage_deferred_provisioning_luks(mount.path(), passphrase, false).unwrap();

        let key = mount.path().join(OMARCHY_PROVISIONING_LUKS_KEY);
        assert_eq!(fs::read(&key).unwrap(), passphrase);
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let initramfs_key = mount.path().join(OMARCHY_PROVISIONING_KEYFILE);
        assert_eq!(fs::read(&initramfs_key).unwrap(), passphrase);
        assert_eq!(
            fs::metadata(&initramfs_key).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let limine_dropin = mount.path().join(OMARCHY_PROVISIONING_LIMINE_UNLOCK_DROPIN);
        assert_eq!(
            fs::read_to_string(&limine_dropin).unwrap(),
            "KERNEL_CMDLINE[default]+=\" cryptkey=rootfs:/etc/omarchy/provisioning.key\"\n"
        );

        let mkinitcpio_dropin = mount.path().join(OMARCHY_PROVISIONING_MKINITCPIO_DROPIN);
        assert_eq!(
            fs::read_to_string(&mkinitcpio_dropin).unwrap(),
            "FILES+=(/etc/omarchy/provisioning.key)\n"
        );
    }

    #[test]
    fn stage_deferred_provisioning_luks_writes_no_trailing_newline() {
        let mount = tempfile::tempdir().unwrap();
        stage_deferred_provisioning_luks(mount.path(), b"pass", false).unwrap();
        let key = mount.path().join(OMARCHY_PROVISIONING_LUKS_KEY);
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
        remove_staged_file(&path, false).unwrap();
    }

    #[test]
    fn configure_repo_replaces_host_mirror() {
        let config = "[core]\nServer = https://archlinux.org/$arch\n\n[omarchy]\nServer = https://stable-mirror.omarchy.org/$arch\n\n[extra]\n";
        let configured = configure_repo(config);
        assert!(configured.contains("[omarchy]\nServer = https://pkgs.omarchy.org/stable/$arch"));
        assert!(!configured.contains("stable-mirror.omarchy.org"));
        assert_eq!(configured.matches("[omarchy]").count(), 1);
        assert!(configured.contains("[extra]"));
    }

    #[test]
    fn configure_repo_appends_when_missing() {
        let configured = configure_repo("[core]\nServer = https://archlinux.org/$arch\n");
        assert!(configured.ends_with(
            "\n[omarchy]\nServer = https://pkgs.omarchy.org/stable/$arch\nSigLevel = Optional TrustAll\n"
        ));
    }

    #[test]
    fn configure_official_repos_forces_stable_mirror() {
        let configured = configure_official_repos(
            "[options]\n\n[core]\nServer = https://archlinux.org/$arch\n\n[core-testing]\nServer = https://testing.archlinux.org/$arch\n\n[extra]\nInclude = /etc/pacman.d/mirrorlist\n\n[custom]\nServer = https://custom.example/$arch\n",
        );
        assert!(configured.contains("[core]\nInclude = /etc/pacman.d/mirrorlist"));
        assert!(configured.contains("[extra]\nInclude = /etc/pacman.d/mirrorlist"));
        assert!(configured.contains("[multilib]\nInclude = /etc/pacman.d/mirrorlist"));
        assert!(configured.contains("[custom]\nServer = https://custom.example/$arch"));
        assert!(!configured.contains("archlinux.org"));
        assert!(!configured.contains("testing.archlinux.org"));
    }

    #[test]
    fn add_pacman_options_keeps_options_in_options_section() {
        let configured = add_pacman_options(
            "[options]\nArchitecture = auto\n\n[core]\nServer = https://archlinux.org/$arch\n",
            &[
                "NoExtract = usr/share/libalpm/hooks/*limine*",
                "NoExtract = etc/pacman.d/hooks/*limine*",
            ],
        );
        assert!(configured.contains(
            "[options]\nArchitecture = auto\n\nNoExtract = usr/share/libalpm/hooks/*limine*\nNoExtract = etc/pacman.d/hooks/*limine*\n[core]"
        ));
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

    #[test]
    fn prioritize_packages_keeps_official_aliases() {
        let base = HashSet::from([
            String::from("yay"),
            String::from("mise-bin"),
            String::from("quickshell-git"),
        ]);
        let requested = vec![
            String::from("yay-bin"),
            String::from("mise"),
            String::from("quickshell"),
            String::from("ripgrep"),
        ];
        assert_eq!(
            prioritize_packages(requested, &base),
            vec![String::from("ripgrep")]
        );
    }

    #[test]
    fn prioritize_packages_keeps_one_supplemental_conflict_choice() {
        let requested = vec![
            String::from("pulseaudio"),
            String::from("pipewire-pulse"),
            String::from("onevpl"),
            String::from("libvpl"),
            String::from("jack2"),
            String::from("pipewire-jack"),
        ];
        assert_eq!(
            prioritize_packages(requested, &HashSet::new()),
            vec![
                String::from("pipewire-pulse"),
                String::from("libvpl"),
                String::from("pipewire-jack"),
            ]
        );
    }
}
