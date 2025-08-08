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

// TODO: Get linux kernel version from the system - support Manjaro
pub const BASE_PACKAGES: [&str; 9] = [
    "base",
    "linux",
    "linux-firmware",
    "grub",
    "efibootmgr",
    "intel-ucode",
    "networkmanager",
    "broadcom-wl",
    "amd-ucode",
];

pub const AUR_DEPENDENCIES: [&str; 3] = ["base-devel", "git", "sudo"];
