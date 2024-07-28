use std::io::{Read, Write};
use std::path::PathBuf;

use fatfs::{format_volume, FileSystem, FormatVolumeOptions, FsOptions};
use log::warn;

pub struct ConfigFs {
    path: PathBuf,
}

pub struct ConfigFsInspect {
    filesystem: FileSystem<std::fs::File>,
}

impl ConfigFs {
    pub fn new(
        path: PathBuf,
        size: u64,
        label: &str,
        template_path: PathBuf,
        substitutions: &[(&str, &str)],
    ) -> std::io::Result<Self> {
        let filesystem = {
            let mut image = std::fs::File::create_new(&path)?;

            image.set_len(size)?;

            let volume_label = {
                let label = label.as_bytes();

                let mut buf = [b' '; 11];
                buf[..label.len()].copy_from_slice(label);
                buf
            };

            let options = FormatVolumeOptions::new().volume_label(volume_label);

            format_volume(&mut image, options)?;

            FileSystem::new(image, FsOptions::new())?
        };

        let root_dir = filesystem.root_dir();

        for entry in std::fs::read_dir(template_path)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let path = entry.path();

            if !entry.metadata()?.is_file() {
                let p = path.display();

                warn!("Ignoring non-file entry '{p}' during assembly of config fs",);
                continue;
            }

            let name = match file_name.to_str() {
                Some(name) => name,
                None => {
                    warn!(
                        "Ignoring file with non-utf8 name '{}' during assembly of config fs",
                        file_name.to_string_lossy()
                    );
                    continue;
                }
            };

            // Replace placeholders in the file, like <REPO_OWNER> or <JITCONFIG>
            // with values provided in `substitutions`.
            // This is not an efficient or elegant solution, but a simple one.
            // This assumes that all files that should be placed in the config
            // filesystems are utf-8 text.

            let mut content = std::fs::read_to_string(path)?;

            for (from, to) in substitutions {
                content = content.replace(from, to);
            }

            let mut file = root_dir.create_file(name)?;
            file.truncate()?;
            file.write_all(content.as_bytes())?;
        }

        std::mem::drop(root_dir);
        filesystem.unmount()?;

        Ok(Self { path })
    }

    pub fn inspect(self) -> std::io::Result<ConfigFsInspect> {
        let filesystem = {
            let image = std::fs::File::options()
                .read(true)
                .write(true)
                .open(&self.path)?;

            FileSystem::new(image, FsOptions::new())?
        };

        Ok(ConfigFsInspect { filesystem })
    }
}

impl Drop for ConfigFs {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).unwrap();
    }
}

impl ConfigFsInspect {
    pub fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        let root_dir = self.filesystem.root_dir();

        let mut buf = Vec::new();

        root_dir.open_file(path)?.read_to_end(&mut buf)?;

        Ok(buf)
    }
}
