use super::Tool;
use crate::args;
use anyhow::{Context, anyhow};
use log::debug;

use std::fs;
use std::os::unix::process::CommandExt as UnixCommandExt;
use std::path::{Path, PathBuf};

/// Loads given block device in qemu
/// Uses kvm if it is enabled
pub fn qemu(command: args::QemuCommand) -> anyhow::Result<()> {
    let qemu = Tool::find("qemu-system-x86_64", false).map_err(|_| {
        anyhow!(
            "qemu-system-x86_64 is required for running the virtual machine.
Please install the 'qemu-desktop', 'qemu-system-x86', and 'edk2-ovmf' packages."
        )
    })?;

    let mut run = qemu.execute();
    run.args([
        "-m",
        "4G",
        "-netdev",
        "user,id=user.0",
        "-device",
        "virtio-net-pci,netdev=user.0",
        "-device",
        "qemu-xhci,id=xhci",
        "-device",
        "usb-tablet,bus=xhci.0",
        "-drive",
    ])
    .arg(format!(
        "file={},if=virtio,format=raw",
        command.block_device.display()
    ))
    .args(command.args);

    let _ovmf_vars = if !command.bios {
        let code = find_ovmf_file("OVMF_CODE.4m.fd").ok_or_else(|| {
            anyhow!("UEFI firmware is required by default. Install 'edk2-ovmf' or pass --bios.")
        })?;
        let vars_template = find_ovmf_file("OVMF_VARS.4m.fd").ok_or_else(|| {
            anyhow!("UEFI variable template is missing. Install or repair the 'edk2-ovmf' package.")
        })?;
        let vars = tempfile::Builder::new()
            .prefix("alma-ovmf-vars-")
            .suffix(".fd")
            .tempfile()
            .context("Failed to create a writable OVMF variables file")?;
        fs::copy(&vars_template, vars.path()).with_context(|| {
            format!(
                "Failed to copy OVMF variables template from {}",
                vars_template.display()
            )
        })?;

        run.args([
            "-drive",
            &format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
            "-drive",
            &format!("if=pflash,format=raw,file={}", vars.path().display()),
        ]);
        Some(vars)
    } else {
        None
    };

    if PathBuf::from("/dev/kvm").exists() {
        debug!("KVM is enabled");
        run.args(["-enable-kvm", "-cpu", "host"]);
    }

    let err = run.exec();

    Err(err).context("Failed launching Qemu")?
}

fn find_ovmf_file(name: &str) -> Option<PathBuf> {
    [
        Path::new("/usr/share/edk2/x64").join(name),
        Path::new("/usr/share/edk2-ovmf/x64").join(name),
    ]
    .into_iter()
    .find(|path| path.is_file())
}
