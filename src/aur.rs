use anyhow::anyhow;
use clap::ValueEnum;
use std::str::FromStr;
use strum::EnumIter;
use strum::IntoEnumIterator;

#[derive(EnumIter, Clone, Debug)]
pub enum AurHelper {
    Paru,
    Yay,
}

impl AurHelper {
    pub fn get_package_name(&self) -> String {
        match self {
            Self::Paru => "paru-bin".to_owned(),
            Self::Yay => "yay-bin".to_owned(),
        }
    }

    pub fn get_install_command(&self) -> Vec<String> {
        match self {
            Self::Paru => vec![
                String::from("paru"),
                String::from("-S"),
                String::from("--skipreview"),
                String::from("--noupgrademenu"),
                String::from("--useask"),
                String::from("--removemake"),
                String::from("--norebuild"),
                String::from("--nocleanafter"),
                String::from("--noredownload"),
                String::from("--mflags"),
                String::from(""),
                String::from("--noconfirm"),
                String::from("--batchinstall"),
            ],
            Self::Yay => vec![
                String::from("yay"),
                String::from("-S"),
                String::from("--useask"),
                String::from("--removemake"),
                String::from("--norebuild"),
                String::from("--answeredit"),
                String::from("None"),
                String::from("--answerclean"),
                String::from("None"),
                String::from("--answerdiff"),
                String::from("None"),
                String::from("--mflags"),
                String::from("--noconfirm"),
            ],
        }
    }
}

impl FromStr for AurHelper {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "paru" => Ok(Self::Paru),
            "yay" => Ok(Self::Yay),
            _ => Err(anyhow!("Error parsing AUR helper string: {}", s)),
        }
    }
}

impl std::fmt::Display for AurHelper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let out = match self {
            Self::Paru => "paru",
            Self::Yay => "yay",
        };
        write!(f, "{out}")
    }
}

impl ValueEnum for AurHelper {
    fn value_variants<'a>() -> &'a [Self] {
        // TODO: Leak necessary?
        Box::leak(Box::new(AurHelper::iter().collect::<Vec<AurHelper>>()))
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        // TODO: Leak necessary?
        let name: &'static str = Box::leak(self.to_string().into_boxed_str());

        Some(clap::builder::PossibleValue::new(name))
    }
}
