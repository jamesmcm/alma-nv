use crate::storage::filesystem::Filesystem;
use anyhow::anyhow;
use log::{debug, warn};
use nix::mount::{MsFlags, mount, umount};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

pub struct MountStack<'a> {
    targets: Vec<PathBuf>,
    _lifetime: PhantomData<&'a ()>, // Changed to a generic lifetime
    dryrun: bool,
}

impl<'a> MountStack<'a> {
    pub fn new(dryrun: bool) -> Self {
        MountStack {
            targets: Vec::new(),
            _lifetime: PhantomData,
            dryrun,
        }
    }

    /// Mounts a single source to a target, with explicit flags and data.
    pub fn mount_single(
        &mut self,
        source: &Path,
        target: &Path,
        fstype: Option<&str>,
        flags: MsFlags,
        data: Option<&str>,
    ) -> nix::Result<()> {
        debug!(
            "Mounting {} to {} (type: {:?}, flags: {:?}, data: {:?})",
            source.display(),
            target.display(),
            fstype.unwrap_or("auto"),
            flags,
            data.unwrap_or("none")
        );
        if !self.dryrun {
            mount(Some(source), target, fstype, flags, data)?;
        } else {
            let type_str = fstype.map_or(String::new(), |t| format!("-t {t}"));
            // In dryrun, we lump flags and data into a single -o for simplicity.
            let opts_str = match (flags.contains(MsFlags::MS_NOATIME), data) {
                (true, Some(d)) => format!("-o noatime,{d}"),
                (true, None) => "-o noatime".to_string(),
                (false, Some(d)) => format!("-o {d}"),
                (false, None) => String::new(),
            };
            println!(
                "mount {} {} {} {}",
                type_str,
                opts_str,
                source.display(),
                target.display()
            );
        }
        self.targets.push(target.to_path_buf());
        Ok(())
    }

    /// Convenience wrapper for mounting a Filesystem object with standard flags.
    pub fn mount(
        &mut self,
        filesystem: &'a Filesystem,
        target: PathBuf,
        extra_flags: MsFlags,
    ) -> nix::Result<()> {
        self.mount_single(
            filesystem.block().path(),
            &target,
            Some(filesystem.fs_type().to_mount_type()),
            extra_flags,
            None,
        )
    }

    pub fn bind_mount(
        &mut self,
        source: PathBuf,
        target: PathBuf,
        options: Option<&str>,
    ) -> nix::Result<()> {
        debug!("Mounting {source:?} to {target:?}");
        if !self.dryrun {
            mount::<_, _, str, _>(
                Some(&source),
                &target,
                None,
                MsFlags::MS_BIND | MsFlags::MS_NOATIME, // Read-only flag has no effect for bind mounts
                options,
            )?;
        } else {
            // TODO: Add flags, etc.
            println!("mount --bind {} {}", source.display(), target.display());
        }
        self.targets.push(target);
        Ok(())
    }

    fn _umount(&mut self) -> anyhow::Result<()> {
        let mut result = Ok(());

        while let Some(target) = self.targets.pop() {
            debug!("Unmounting {}", target.display());

            if !self.dryrun {
                if let Err(e) = umount(&target) {
                    warn!("Unable to umount {}: {}", target.display(), e);
                    result = Err(anyhow!(
                        "Failed unmounting filesystem: {}, {}",
                        target.display(),
                        e
                    ));
                };
            } else {
                println!("umount {}", target.display());
            }
        }

        result
    }

    pub fn umount(mut self) -> anyhow::Result<()> {
        self._umount()
    }
}

impl<'a> Drop for MountStack<'a> {
    fn drop(&mut self) {
        self._umount().ok();
    }
}
