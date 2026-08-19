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

// Omarchy 4 (Quattro) package repository configuration.
// Quattro installs Omarchy as Pacman packages from its own repository rather
// than cloning a git repo and running an installer script.
pub const OMARCHY_DEFAULT_REPO_URL: &str = "https://pkgs.omarchy.org";
pub const OMARCHY_DEFAULT_REPO_NAME: &str = "omarchy";

// The default LUKS mapper name used by Omarchy's own installer. ALMA reuses
// this name so that the boot configuration matches upstream expectations.
pub const OMARCHY_MAPPER_NAME: &str = "omarchy_root";

// Quattro package names (bootstrap these first, then read the manifests).
// The `omarchy` runtime package pulls in the core desktop (hyprland,
// quickshell, uwsm, sddm, pipewire, wireplumber, gnome-keyring, ...) as
// dependencies; omarchy-settings ships the limine/snapper templates and
// /etc/skel defaults.
pub const OMARCHY_PACKAGES: [&str; 4] = [
    "omarchy",
    "omarchy-keyring",
    "omarchy-settings",
    "omarchy-nvim",
];

// Relative paths inside the installed packages that enumerate the packages
// needed to build a full Omarchy desktop. These ship inside the `omarchy`
// runtime package.
pub const OMARCHY_BASE_MANIFEST: &str = "usr/share/omarchy/install/omarchy-base.packages";
pub const OMARCHY_OTHER_MANIFEST: &str = "usr/share/omarchy/install/omarchy-other.packages";

// Paths (inside the installed system) to the Limine templates shipped by
// `omarchy-settings`. These are the source of the boot configuration.
pub const OMARCHY_LIMINE_DEFAULTS: &str = "usr/share/omarchy/default/limine/default.conf";
pub const OMARCHY_LIMINE_CONF: &str = "usr/share/omarchy/default/limine/limine.conf";

// Deferred-provisioning state paths, matching what the Quattro runtime expects.
pub const OMARCHY_PROVISIONING_DIR: &str = "var/lib/omarchy/provisioning";
pub const OMARCHY_PROVISIONING_PENDING: &str = "var/lib/omarchy/provisioning/pending";
pub const OMARCHY_PROVISIONING_LUKS_KEY: &str = "var/lib/omarchy/provisioning/luks-key";
pub const OMARCHY_PROVISIONING_KEYFILE: &str = "etc/omarchy/provisioning.key";
pub const OMARCHY_PROVISIONING_LIMINE_UNLOCK_DROPIN: &str =
    "etc/limine-entry-tool.d/99-omarchy-provisioning-unlock.conf";
pub const OMARCHY_PROVISIONING_MKINITCPIO_DROPIN: &str =
    "etc/mkinitcpio.conf.d/99-omarchy-provisioning-key.conf";
pub const OMARCHY_PROVISION_OWNER_SERVICE: &str =
    "usr/share/omarchy/install/provisioning/omarchy-provision-owner.service";
pub const OMARCHY_PROVISION_OWNER_UNIT: &str = "etc/systemd/system/omarchy-provision-owner.service";

// Host-specific hardware configuration files that `omarchy-apply-hardware` may
// write for the *build* host. A portable image must not carry these: they force
// machine-specific initramfs modules or hide the generic `linux` kernel from
// the boot menu on other machines.
pub const OMARCHY_PORTABLE_MKINITCPIO_DROPINS: &[&str] = &[
    "nvidia.conf",
    "apple-t2.conf",
    "macbook_spi_modules.conf",
    "surface_device_modules.conf",
];

pub const OMARCHY_PORTABLE_LIMINE_DROPINS: &[&str] = &[
    "zz-dell-xps-panther-lake.conf",
    "dell-xps-panther-lake.conf",
    "t2-mac.conf",
    "asus-ptl-display-backlight.conf",
    "asus-expertbook-b9406-display.conf",
    "intel-panther-lake-fred.conf",
];

pub const OMARCHY_PORTABLE_MODPROBE_FILES: &[&str] = &["nvidia.conf"];

// A conservative generic hardware profile used for portable installations that
// may boot on arbitrary machines. These packages are safe to pre-install: they
// only activate when matching hardware is present.
pub const OMARCHY_PORTABLE_PACKAGES: [&str; 17] = [
    "intel-ucode",
    "amd-ucode",
    "linux-firmware",
    "mesa",
    "lib32-mesa",
    "vulkan-intel",
    "lib32-vulkan-intel",
    "vulkan-radeon",
    "lib32-vulkan-radeon",
    "nvidia-open-dkms",
    "nvidia-utils",
    "lib32-nvidia-utils",
    "libva-nvidia-driver",
    "sof-firmware",
    "linux",
    "linux-headers",
    "linux-firmware-marvell",
];

// A curated "Omarchy core" package set for the portable profile. This gives a
// genuine, fully-functioning Omarchy desktop (Hyprland + Quickshell shell +
// Omarchy runtime + settings, terminal, login, networking, audio, fonts/theme,
// browser and essential utilities) while deliberately leaving out the heavy
// optional workstation applications that make up the rest of the official
// omarchy-base.packages manifest (office suites, Docker, media apps, gaming,
// etc.).
//
// The `omarchy` package is installed separately and pulls in the core desktop
// dependencies (hyprland, quickshell, uwsm, sddm, pipewire, wireplumber,
// xdg-desktop-portal-hyprland, gnome-keyring, gum, jq, git, ...).
pub const OMARCHY_CORE_PACKAGES: [&str; 69] = [
    // terminal + session machinery
    "foot",
    "xdg-terminal-exec",
    "uwsm",
    // browser (Omarchy's default)
    "chromium",
    // fonts / theme
    "noto-fonts",
    "noto-fonts-cjk",
    "noto-fonts-emoji",
    "ttf-jetbrains-mono-nerd-basic",
    "woff2-font-awesome",
    "yaru-icon-theme",
    "gnome-themes-extra",
    // audio
    "pipewire",
    "pipewire-alsa",
    "pipewire-jack",
    "pipewire-pulse",
    "gst-plugin-pipewire",
    "libpulse",
    "wireplumber",
    "pamixer",
    "alsa-utils",
    "sof-firmware",
    // bluetooth
    "bluez",
    "bluez-utils",
    "bluez-tools",
    // desktop integration / input
    "xdg-desktop-portal-gtk",
    "xdg-desktop-portal-hyprland",
    "gnome-keyring",
    "libsecret",
    "fcitx5",
    "fcitx5-gtk",
    "fcitx5-qt",
    "power-profiles-daemon",
    "brightnessctl",
    "udiskie",
    "xdg-user-dirs",
    // essential utilities
    "bat",
    "eza",
    "fd",
    "fzf",
    "ripgrep",
    "zoxide",
    "starship",
    "tmux",
    "less",
    "man-db",
    "fastfetch",
    "btop",
    "unzip",
    "rsync",
    "pacman-contrib",
    // screen capture / clipboard
    "grim",
    "slurp",
    "hyprpicker",
    "wl-clipboard",
    "wtype",
    "imagemagick",
    // system infrastructure required by Omarchy's own apply-system
    // (config/enable-services.sh, docker.sh and firewall.sh). These are system
    // daemons rather than the optional GUI applications the portable profile
    // trims, so they must stay for `omarchy-apply-system` to succeed.
    "cups",
    "cups-browsed",
    "cups-filters",
    "cups-pdf",
    "avahi",
    "nss-mdns",
    "kernel-modules-hook",
    "docker",
    "docker-buildx",
    "docker-compose",
    "ufw",
    "ufw-docker",
    "plocate",
];

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
