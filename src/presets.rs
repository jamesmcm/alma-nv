use anyhow::{Context, anyhow};
use either::Either;
use flate2::read::GzDecoder;
use reqwest::Url;
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::fs::DirEntry;
use std::io;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use zip::ZipArchive;

#[derive(Debug, Clone)]
pub enum PresetsPath {
    LocalDir(PathBuf),
    LocalArchive(PathBuf, ArchiveType),
    UrlArchive(Url, ArchiveType),
    GitHttp(Url),
    GitSSH(String), // TODO: Use better type here
}

#[derive(Debug)]
pub enum PathWrapper {
    Path(PathBuf),
    Tmp(TempDir),
}

#[derive(Debug, Clone, Copy)]
pub enum ArchiveType {
    Zip,
    TarGz,
}

trait ReadSeek: io::Read + io::Seek {}
impl<T> ReadSeek for T where T: io::Read + io::Seek {}

impl ArchiveType {
    pub fn extract_to_dir(
        &self,
        archive: Either<&Path, bytes::Bytes>,
        dir: &Path,
    ) -> anyhow::Result<()> {
        let reader: Box<dyn ReadSeek> = if let Either::Left(p) = archive {
            Box::new(std::fs::File::open(p)?)
        } else {
            Box::new(std::io::Cursor::new(archive.right().unwrap()))
        };

        match self {
            ArchiveType::Zip => {
                let mut zip = ZipArchive::new(reader)?;
                zip.extract(dir)?;
                Ok(())
            }
            ArchiveType::TarGz => {
                let tar = GzDecoder::new(reader);
                let mut archive_file = tar::Archive::new(tar);
                archive_file.unpack(dir)?;
                Ok(())
            }
        }
    }
}

impl PathWrapper {
    pub fn to_path(&self) -> &std::path::Path {
        match self {
            PathWrapper::Path(p) => p.as_path(),
            PathWrapper::Tmp(t) => t.path(),
        }
    }
}

impl PresetsPath {
    // Consumes the PresetsPath and retuns either a PathBuf or a TempDir
    pub fn into_path_wrapper(self, noconfirm: bool) -> anyhow::Result<PathWrapper> {
        match self {
            // if local dir / file then return that
            PresetsPath::LocalDir(p) => Ok(PathWrapper::Path(p)),
            // If local archive then extract to tmpfile dir
            PresetsPath::LocalArchive(p, archive_type) => {
                let tmpdir = tempfile::tempdir()?;

                archive_type.extract_to_dir(Either::Left(p.as_path()), tmpdir.path())?;

                // TODO: Verify contents of archive
                Ok(PathWrapper::Tmp(tmpdir))
            }
            // If url archive then download with reqwest and extract to tmpfile dir
            PresetsPath::UrlArchive(u, archive_type) => {
                let resp = reqwest::blocking::Client::new().get(u).send()?;
                let bytes = resp.bytes()?;
                let tmpdir = tempfile::tempdir()?;

                archive_type.extract_to_dir(Either::Right(bytes), tmpdir.path())?;
                Ok(PathWrapper::Tmp(tmpdir))
            }
            // If git then clone to tmpfile dir
            PresetsPath::GitHttp(u) => {
                let tmpdir = tempfile::tempdir()?;
                git2::Repository::clone(u.as_str(), tmpdir.path())?;
                Ok(PathWrapper::Tmp(tmpdir))
            }
            PresetsPath::GitSSH(u) => {
                // Prepare callbacks.
                let mut callbacks = git2::RemoteCallbacks::new();
                // TODO: Get SSH key path

                let mut ssh_keys: Vec<DirEntry> =
                    std::fs::read_dir(Path::new(&format!("{}/.ssh/", env::var("HOME")?)))?
                        .filter_map(|f| {
                            f.ok().and_then(|fi| {
                                if fi.path().is_file()
                                    && fi.file_name().to_string_lossy().starts_with("id")
                                    && !fi.file_name().to_string_lossy().ends_with(".pub")
                                {
                                    Some(fi)
                                } else {
                                    None
                                }
                            })
                        })
                        .collect();

                // TODO: Improve error handling
                ssh_keys.sort_by(|a, b| {
                    b.metadata()
                        .unwrap()
                        .modified()
                        .unwrap()
                        .cmp(&a.metadata().unwrap().modified().unwrap())
                });

                dbg!(&ssh_keys);

                let password = if noconfirm {
                    String::new()
                } else {
                    dialoguer::Password::new()
                        .with_prompt("Enter SSH key password")
                        .allow_empty_password(true)
                        .interact()?
                };

                // TODO: Improve error handling
                callbacks.credentials(move |_url, username_from_url, _allowed_types| {
                    let username = username_from_url.ok_or_else(|| {
                        git2::Error::from_str("SSH URL does not contain a username")
                    })?;
                    let key_path = match ssh_keys.first() {
                        Some(entry) => entry.path(),
                        None => {
                            return Err(git2::Error::from_str(
                                "No suitable SSH keys found in ~/.ssh/",
                            ));
                        }
                    };
                    git2::Cred::ssh_key(
                        username,
                        None,
                        &key_path,
                        if !password.is_empty() {
                            Some(&password)
                        } else {
                            None
                        },
                    )
                });

                // Prepare fetch options.
                let mut fo = git2::FetchOptions::new();
                fo.remote_callbacks(callbacks);

                // Prepare builder.
                let mut builder = git2::build::RepoBuilder::new();
                builder.fetch_options(fo);

                let tmpdir = tempfile::tempdir()?;
                // Clone the project.
                builder.clone(u.as_str(), tmpdir.path())?;

                Ok(PathWrapper::Tmp(tmpdir))
            }
        }
    }
}

impl std::str::FromStr for PresetsPath {
    type Err = String;

    // TODO: Improve error handling
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.starts_with("http://") || s.starts_with("https://") {
            if s.ends_with(".zip") {
                Ok(Self::UrlArchive(
                    Url::parse(s).map_err(|e| e.to_string())?,
                    ArchiveType::Zip,
                ))
            } else if s.ends_with(".tar.gz") {
                Ok(Self::UrlArchive(
                    Url::parse(s).map_err(|e| e.to_string())?,
                    ArchiveType::TarGz,
                ))
            } else if s.ends_with(".git") {
                Ok(Self::GitHttp(Url::parse(s).map_err(|e| e.to_string())?))
            } else {
                Err(format!("Could not parse URL: {}", &s))
            }
        } else if (s.starts_with("git@") || s.starts_with("ssh://")) && s.ends_with(".git") {
            Ok(Self::GitSSH(s.to_string()))
        } else {
            // TODO: Check if valid path
            // TODO: Improve archive detection - check MIME ?
            if s.ends_with(".zip") {
                Ok(Self::LocalArchive(
                    PathBuf::from_str(s).map_err(|e| e.to_string())?,
                    ArchiveType::Zip,
                ))
            } else if s.ends_with(".tar.gz") {
                Ok(Self::LocalArchive(
                    PathBuf::from_str(s).map_err(|e| e.to_string())?,
                    ArchiveType::TarGz,
                ))
            } else {
                Ok(Self::LocalDir(
                    PathBuf::from_str(s).map_err(|e| e.to_string())?,
                ))
            }
        }
    }
}

#[derive(Deserialize, Debug)]
struct Preset {
    packages: Option<Vec<String>>,
    script: Option<String>,
    environment_variables: Option<Vec<String>>,
    shared_directories: Option<Vec<PathBuf>>,
    aur_packages: Option<Vec<String>>,
}

fn visit_dirs(dir: &Path, filevec: &mut Vec<PathBuf>) -> Result<(), io::Error> {
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                visit_dirs(&path, filevec)?;
            } else if entry.path().extension() == Some(&std::ffi::OsString::from("toml")) {
                filevec.push(entry.path());
            }
        }
    }
    Ok(())
}

impl Preset {
    fn load(path: &Path) -> anyhow::Result<Self> {
        let data = fs::read_to_string(path).with_context(|| format!("{}", path.display()))?;
        toml::from_str(&data).with_context(|| format!("{}", path.display()))
    }

    fn process(
        &self,
        packages: &mut HashSet<String>,
        scripts: &mut Vec<Script>,
        environment_variables: &mut HashSet<String>,
        path: &Path,
        aur_packages: &mut HashSet<String>,
    ) -> anyhow::Result<()> {
        if let Some(preset_packages) = &self.packages {
            packages.extend(preset_packages.clone());
        }

        if let Some(preset_aur_packages) = &self.aur_packages {
            aur_packages.extend(preset_aur_packages.clone());
        }

        if let Some(preset_environment_variables) = &self.environment_variables {
            environment_variables.extend(preset_environment_variables.clone());
        }

        if let Some(script_text) = &self.script {
            scripts.push(Script {
                script_text: script_text.clone(),
                shared_dirs: self
                    .shared_directories
                    .clone()
                    .map(|x| {
                        // Convert directories to absolute paths
                        // If any shared directory is not a directory then throw an error
                        x.iter()
                            .cloned()
                            .map(|y| {
                                let full_path = path.parent().expect("Path has no parent").join(&y);
                                if full_path.is_dir() {
                                    Ok(full_path)
                                } else {
                                    Err(anyhow!(
                                        "Preset: {} - shared directory: {} is not directory",
                                        path.display(),
                                        y.display()
                                    ))
                                }
                            })
                            .collect::<anyhow::Result<Vec<_>>>()
                    })
                    .map_or(Ok(None), |r| r.map(Some))?,
            });
        }
        Ok(())
    }
}

pub struct Script {
    pub script_text: String,
    pub shared_dirs: Option<Vec<PathBuf>>,
}

pub struct PresetsCollection {
    pub packages: HashSet<String>,
    pub aur_packages: HashSet<String>,
    pub scripts: Vec<Script>,
}

impl PresetsCollection {
    pub fn load(list: &[&Path]) -> anyhow::Result<Self> {
        let mut packages = HashSet::new();
        let mut aur_packages = HashSet::new();
        let mut scripts: Vec<Script> = Vec::new();
        let mut environment_variables = HashSet::new();

        for preset in list {
            if preset.is_dir() {
                // Build vector of paths to files, then sort by path name
                // Recursively load directories of preset files
                let mut dir_paths: Vec<PathBuf> = Vec::new();
                visit_dirs(preset, &mut dir_paths)
                    .with_context(|| format!("{}", preset.display()))?;

                // Order not guaranteed so we sort
                // In the future may want to support numerical sort i.e. 15_... < 100_...
                dir_paths.sort();

                for path in dir_paths {
                    // Note any errant TOML file will cause the entire process to fail
                    Preset::load(&path)?.process(
                        &mut packages,
                        &mut scripts,
                        &mut environment_variables,
                        &path,
                        &mut aur_packages,
                    )?;
                }
            } else {
                Preset::load(preset)?.process(
                    &mut packages,
                    &mut scripts,
                    &mut environment_variables,
                    preset,
                    &mut aur_packages,
                )?;
            }
        }
        let missing_envrionments: Vec<String> = environment_variables
            .into_iter()
            .filter(|var| env::var(var).is_err())
            .collect();

        if !missing_envrionments.is_empty() {
            return Err(anyhow!(
                "Missing environment variables {:?}",
                missing_envrionments
            ));
        }

        Ok(Self {
            packages,
            aur_packages,
            scripts,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_presetspath_localpath() {
        let path = PathBuf::from_str("/path/test").unwrap();
        let pp = PresetsPath::LocalDir(path.clone());
        if let PathWrapper::Path(p) = pp.clone().into_path_wrapper(false).unwrap() {
            assert_eq!(p, path)
        } else {
            panic!("Expected PathWrapper::Path")
        }

        assert_eq!(
            path.as_path(),
            pp.into_path_wrapper(false).unwrap().to_path()
        );
    }
}
