//! System-variant policy for the common ALMA installation pipeline.
//!
//! The storage, filesystem, and mount lifecycle is shared by all ALMA
//! targets.  The two supported systems differ in package bootstrap,
//! repository configuration, bootloader, and post-install configuration;
//! those differences live behind [`SystemInstaller`] in the sibling modules.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use byte_unit::Byte;
use tempfile::TempDir;

use crate::args::{CreateCommand, Source, SystemVariant};
use crate::interactive::UserSettings;
use crate::presets::PresetsCollection;
use crate::tool::{Tool, Tools};

pub mod archlinux;
pub mod omarchy;

/// The variant-specific half of the ALMA create pipeline.
///
/// The interface deliberately operates on pipeline contexts instead of
/// exposing a large list of positional arguments at every call site.  This
/// keeps `create.rs` focused on the lifecycle that both systems share:
/// select a target, partition it, mount it, bootstrap it, and unmount it.
pub trait SystemInstaller: Sync {
    fn validate_command(&self, command: &CreateCommand) -> Result<()>;
    fn adjust_command(&self, command: &mut CreateCommand) -> Result<()>;
    fn capture_luks_passphrase(&self, command: &CreateCommand) -> Result<Option<Vec<u8>>>;
    fn validate_target_size(&self, command: &CreateCommand, total_size: Byte) -> Result<()>;

    fn default_boot_size_mb(&self) -> u32;
    fn validate_boot_size(&self, command: &CreateCommand, boot_size_mb: u32) -> Result<()>;
    fn mapper_name(&self) -> &'static str;

    fn bootstrap_packages(&self, context: &BootstrapContext<'_>) -> HashSet<String>;
    fn prepare_bootstrap(&self, mount_path: &Path, dryrun: bool) -> Result<()>;
    fn configure_bootstrap(&self, base_conf: &Path) -> Result<BootstrapConfig>;
    fn complete_bootstrap(
        &self,
        context: &BootstrapContext<'_>,
        config: &BootstrapConfig,
    ) -> Result<()>;

    fn apply_customizations(&self, context: &CustomizationContext<'_>) -> Result<()>;
    fn finalize(&self, context: &FinalizeContext<'_>) -> Result<()>;
    fn add_manifest_sources(&self, sources: &mut Vec<Source>);

    /// Whether the variant writes its own authoritative `/etc/fstab` during
    /// `finalize` (e.g. Omarchy mirrors Quattro's pre-mounted layout exactly).
    /// When true, the shared pipeline skips genfstab entirely instead of
    /// producing an fstab that would be overwritten.
    fn provides_own_fstab(&self) -> bool {
        false
    }
}

/// Selects the one system implementation used for the whole create run.
pub fn installer(variant: SystemVariant) -> &'static dyn SystemInstaller {
    match variant {
        SystemVariant::Arch => &archlinux::INSTALLER,
        SystemVariant::Omarchy => &omarchy::INSTALLER,
    }
}

pub struct BootstrapContext<'a> {
    pub command: &'a CreateCommand,
    pub tools: &'a Tools,
    pub mount_path: &'a Path,
    pub presets: &'a PresetsCollection,
    pub user_settings: Option<&'a UserSettings>,
    pub portable_target: bool,
}

/// Pacman configuration selected for the bootstrap transaction.
pub struct BootstrapConfig {
    pub pacman_conf: PathBuf,
    /// The pacman.conf to leave inside the installed system. Each variant
    /// owns copying or generating this config during `complete_bootstrap`;
    /// generic Arch preserves the selected config, while Omarchy installs its
    /// generated target config before in-target transactions.
    pub target_pacman_conf: PathBuf,
    pub use_host_cache: bool,
    pub use_host_mirrorlist: bool,
    // Omarchy's isolated configuration is stored in a temporary directory;
    // keep that directory alive until all bootstrap transactions finish.
    pub(crate) _temp_dir: Option<TempDir>,
}

impl BootstrapConfig {
    pub fn host(pacman_conf: PathBuf) -> Self {
        Self {
            target_pacman_conf: pacman_conf.clone(),
            pacman_conf,
            use_host_cache: true,
            use_host_mirrorlist: true,
            _temp_dir: None,
        }
    }
}

pub struct CustomizationContext<'a> {
    pub command: &'a CreateCommand,
    pub arch_chroot: &'a Tool,
    pub presets: &'a PresetsCollection,
    pub mount_path: &'a Path,
}

pub struct FinalizeContext<'a> {
    pub command: &'a CreateCommand,
    pub tools: &'a Tools,
    pub mount_point: &'a TempDir,
    pub storage_device_path: &'a Path,
    pub boot_partition_path: Option<&'a Path>,
    pub encrypted_root: bool,
    pub root_partition_path: &'a Path,
    pub username: Option<&'a str>,
    pub luks_passphrase: Option<&'a [u8]>,
    pub portable_target: bool,
}
