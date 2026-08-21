pub const BOOT_PARTITION_INDEX: u8 = 1;
pub const ROOT_PARTITION_INDEX: u8 = 3;

pub const MIN_BOOT_MB: u32 = 200;
pub const DEFAULT_BOOT_MB: u32 = 300;
pub const MAX_BOOT_MB: u32 = 2048; // 2GiB

pub static JOURNALD_CONF: &str = "
[Journal]
Storage=volatile
SystemMaxUse=16M
";

// AUR dependencies for installing AUR helper
pub const AUR_DEPENDENCIES: [&str; 1] = ["sudo"];

pub const PORTABLE_ZRAM_CONFIG: &str = "[zram0]\n\
zram-size = min(ram / 2, 2048)\n\
compression-algorithm = zstd\n\
swap-priority = 100\n";

pub const PORTABLE_SYSCTL_CONFIG: &str = "vm.swappiness = 150\n\
vm.vfs_cache_pressure = 50\n\
vm.page-cluster = 0\n";

pub const PORTABLE_PSD_CONFIG: &str = "# ALMA portable-media defaults. profile-sync-daemon keeps browser profiles in\n\
# per-user tmpfs and synchronizes them back to disk periodically.\n\
BROWSERS=(firefox chromium)\n\
USE_BACKUPS=\"yes\"\n\
BACKUP_LIMIT=2\n";

pub const FONT_PACKAGES: &[(&str, &[&str])] = &[
    (
        "Noto Fonts (Recommended)",
        &[
            "noto-fonts",
            "noto-fonts-extra",
            "noto-fonts-cjk",
            "noto-fonts-emoji",
        ],
    ),
    ("Liberation Fonts", &["ttf-liberation"]),
    ("Dejavu Fonts", &["ttf-dejavu"]),
    ("Nerd Fonts Complete", &["nerd-fonts-complete"]),
    ("IBM Plex Fonts", &["ttf-ibm-plex"]),
];

pub const VIDEO_PACKAGES: &[(&str, &[&str])] = &[
    (
        "AMD/Intel (Mesa)",
        &[
            "mesa",
            // "xf86-video-amdgpu",
            // "xf86-video-intel",
            // "xf86-video-ati",
        ],
    ),
    ("NVIDIA Proprietary", &["nvidia-dkms"]),
    ("NVIDIA Open Source", &["nvidia-open-dkms"]),
    (
        "Nouveau (Legacy Open Source NVIDIA)",
        &["xf86-video-nouveau"],
    ),
];
