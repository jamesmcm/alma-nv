use crate::constants::{FONT_PACKAGES, VIDEO_PACKAGES};
use anyhow::{Context, anyhow};
use dialoguer::{Confirm, Input, MultiSelect, Password, theme::ColorfulTheme};
use log::{info, warn};
use std::env;

// Struct to hold all collected user settings
#[derive(Debug, Clone)]
pub struct UserSettings {
    pub username: String,
    pub hostname: String,
    pub user_password: Option<String>,
    pub passwordless_sudo: bool,
    pub timezone: String,
    pub graphics_packages: Vec<String>,
    pub font_packages: Vec<String>,
}

impl UserSettings {
    /// Dispatches to either interactive or non-interactive setup.
    pub fn new(noconfirm: bool) -> anyhow::Result<Self> {
        if noconfirm {
            Self::non_interactive()
        } else {
            Self::interactive()
        }
    }

    /// Gathers settings from environment variables with sane defaults.
    fn non_interactive() -> anyhow::Result<Self> {
        info!("Running in non-interactive mode. Using environment variables or defaults.");

        let username = env::var("ALMA_USERNAME").unwrap_or_else(|_| whoami::username());
        warn!("Using username: {}", username);

        let hostname = env::var("ALMA_HOSTNAME").unwrap_or_else(|_| "alma-linux".to_string());
        warn!("Using hostname: {}", hostname);

        // In non-interactive mode, we default to passwordless sudo for safety.
        let passwordless_sudo = env::var("ALMA_PASSWORDLESS_SUDO")
            .map(|v| v.to_lowercase() == "true" || v == "1")
            .unwrap_or(true);
        if passwordless_sudo {
            warn!("Configuring passwordless sudo for user '{}'.", username);
        }

        Ok(Self {
            username,
            hostname,
            user_password: None, // No password set for the user
            passwordless_sudo,
            timezone: env::var("ALMA_TIMEZONE").unwrap_or_else(|_| "UTC".to_string()),
            graphics_packages: VIDEO_PACKAGES[0].1.iter().map(|s| s.to_string()).collect(), // Default to Mesa
            font_packages: FONT_PACKAGES[0].1.iter().map(|s| s.to_string()).collect(), // Default to Noto
        })
    }

    /// Prompts the user interactively for all settings.
    fn interactive() -> anyhow::Result<Self> {
        info!("Starting interactive setup...");

        let username = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Enter username")
            .default(whoami::username())
            .validate_with(validate_username)
            .interact_text()?;

        let hostname = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Enter hostname")
            .default("alma-linux".to_string())
            .validate_with(|s: &str| {
                if s.is_empty() {
                    Err("Hostname cannot be empty")
                } else {
                    Ok(())
                }
            })
            .interact_text()?;

        let user_password = Some(
            Password::with_theme(&ColorfulTheme::default())
                .with_prompt(format!("Enter password for user '{}'", username))
                .with_confirmation("Confirm password", "Passwords do not match.")
                .interact()?,
        );

        let passwordless_sudo = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Enable passwordless sudo for this user?")
            .default(false)
            .interact()?;

        let timezone = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Enter timezone (e.g., Europe/London, America/New_York, or UTC)")
            .default("UTC".to_string())
            .interact_text()?;

        let (graphics_packages, font_packages) = Self::prompt_package_selections()?;

        Ok(Self {
            username,
            hostname,
            user_password,
            passwordless_sudo,
            timezone,
            graphics_packages,
            font_packages,
        })
    }

    fn prompt_package_selections() -> anyhow::Result<(Vec<String>, Vec<String>)> {
        // Graphics drivers
        let video_items: Vec<&str> = VIDEO_PACKAGES.iter().map(|(name, _)| *name).collect();
        let video_defaults = [true, false, false, false]; // Default to Mesa
        let video_selections = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Select graphics drivers (Mesa is recommended)")
            .items(&video_items)
            .defaults(&video_defaults)
            .interact()?;

        let mut selected_video = Vec::new();
        let mut nvidia_selected = false;
        for i in video_selections {
            selected_video.extend(VIDEO_PACKAGES[i].1.iter().map(|s| s.to_string()));
            if i == 1 || i == 2 {
                // nvidia or nvidia-open
                nvidia_selected = true;
            }
        }
        if nvidia_selected {
            selected_video.push("nvidia-utils".to_string());
        }

        // Fonts
        let font_items: Vec<&str> = FONT_PACKAGES.iter().map(|(name, _)| *name).collect();
        let font_defaults = [true, false, false, false, false]; // Default to Noto
        let font_selections = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Select font packages")
            .items(&font_items)
            .defaults(&font_defaults)
            .interact()?;

        let selected_fonts = font_selections
            .into_iter()
            .flat_map(|i| FONT_PACKAGES[i].1.iter().map(|s| s.to_string()))
            .collect();

        Ok((selected_video, selected_fonts))
    }

    /// Generates a bash script string to perform user setup, sudo configuration, and timezone
    pub fn generate_setup_script(&self, arch_chroot: &Tool) -> anyhow::Result<String> {
        let mut script = String::new();
        script.push_str("set -eux\n");

        // Set hostname
        script.push_str(&format!("echo {} > /etc/hostname\n", self.hostname));

        // Timezone
        script.push_str(&format!(
            "ln -sf /usr/share/zoneinfo/{} /etc/localtime\n",
            self.timezone
        ));

        // Create user
        script.push_str(&format!("useradd -m -G wheel {} || true\n", self.username));

        // Set user password
        if let Some(password) = &self.user_password {
            script.push_str(&format!(
                "echo \"{0}:{1}\" | chpasswd\n",
                self.username, password
            ));
        }

        // Sudo configuration
        if self.passwordless_sudo {
            script.push_str("echo '%wheel ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/wheel\n");
        } else {
            script.push_str("echo '%wheel ALL=(ALL) ALL' > /etc/sudoers.d/wheel\n");
        }

        // Enable user dirs update for common desktop environments
        if let Ok(xdg_dirs) = arch_chroot
            .execute()
            .arg("/bin/bash")
            .arg("-c")
            .arg(format!(
                "sudo -u {} which xdg-user-dirs-update",
                self.username
            ))
            .run_text_output(false)
        {
            if !xdg_dirs.trim().is_empty() {
                script.push_str(&format!("sudo -u {} xdg-user-dirs-update\n", self.username));
            }
        }

        Ok(script)
    }
}

fn validate_username(input: &str) -> Result<(), &str> {
    if input.is_empty()
        || input
            .chars()
            .any(|c| !c.is_ascii_lowercase() || !c.is_ascii_alphanumeric() && c != '_')
        || input.len() > 32
    {
        Err("Invalid username: must be all lowercase, alphanumeric, <= 32 chars.")
    } else {
        Ok(())
    }
}
