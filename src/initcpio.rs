use std::fmt::Write;

pub struct Initcpio {
    encrypted: bool,
    plymouth: bool,
}

impl Initcpio {
    pub fn new(encrypted: bool, plymouth: bool) -> Self {
        Self {
            encrypted,
            plymouth,
        }
    }

    pub fn to_config(&self) -> anyhow::Result<String> {
        // Note we do not use autodetect as for USB drives we will boot on different hardware than the image was built on!
        let mut output = String::from(
            "MODULES=()
BINARIES=()
FILES=()
HOOKS=(base udev keyboard microcode modconf keymap consolefont block ",
        );

        if self.encrypted {
            output.write_str("encrypt ")?;
        }

        if self.plymouth {
            output.write_str("kms plymouth")?;
        }

        output.write_str("filesystems fsck)\n")?;

        Ok(output)
    }
}
