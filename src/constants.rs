pub const BOOT_PARTITION_INDEX: u8 = 1;
pub const ROOT_PARTITION_INDEX: u8 = 3;

pub const MIN_BOOT_MB: u32 = 200;
pub const DEFAULT_BOOT_MB: u32 = 300;
pub const MAX_BOOT_MB: u32 = 2048; // 2GiB

pub const OMARCHY_DEFAULT_BOOT_MB: u32 = 512;
pub const OMARCHY_MIN_BOOT_MB: u32 = 512;
pub const OMARCHY_MIN_TOTAL_GIB: u64 = 15;

pub static JOURNALD_CONF: &str = "
[Journal]
Storage=volatile
SystemMaxUse=16M
";

// Base packages for all installations
pub const BASE_PACKAGES: [&str; 13] = [
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

// AUR dependencies for installing AUR helper
pub const AUR_DEPENDENCIES: [&str; 1] = ["sudo"];

pub const OMARCHY_DEFAULT_REPO: &str = "https://github.com/basecamp/omarchy.git";
pub const OMARCHY_DEFAULT_BRANCH: &str = "master";

pub fn omarchy_repo_url() -> String {
    std::env::var("OMARCHY_REPO").unwrap_or_else(|_| OMARCHY_DEFAULT_REPO.to_string())
}

pub fn omarchy_branch() -> String {
    std::env::var("OMARCHY_REF").unwrap_or_else(|_| OMARCHY_DEFAULT_BRANCH.to_string())
}

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
