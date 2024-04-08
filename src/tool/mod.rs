mod chroot;
mod mount;
mod qemu;

use anyhow::Context;
pub use chroot::chroot;
pub use mount::mount;
pub use qemu::qemu;

use std::path::PathBuf;
use std::process::Command;
use which::which;

#[derive(Debug)]
pub struct Tool {
    exec: PathBuf,
    pub dryrun: bool,
}

impl Tool {
    pub fn find(name: &'static str, dryrun: bool) -> anyhow::Result<Self> {
        Ok(Self {
            exec: which(name).context(format!("Cannot find {}", name))?,
            dryrun,
        })
    }

    pub fn execute(&self) -> Command {
        Command::new(&self.exec)
    }
}
