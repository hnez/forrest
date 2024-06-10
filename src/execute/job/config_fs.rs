use std::io::{Read, Write};
use std::path::PathBuf;

use fatfs::{format_volume, FileSystem, FormatVolumeOptions, FsOptions};

pub struct ConfigFsBuilder {
    path: PathBuf,
    filesystem: FileSystem<std::fs::File>,
}

pub struct ConfigFs {
    path: PathBuf,
}

pub struct ConfigFsInspect {
    filesystem: FileSystem<std::fs::File>,
}

impl ConfigFsBuilder {
    pub fn add_file(self, path: &str, content: &[u8]) -> std::io::Result<Self> {
        {
            let root_dir = self.filesystem.root_dir();

            let mut file = root_dir.create_file(path)?;
            file.truncate()?;
            file.write_all(content)?;
        }

        Ok(self)
    }

    pub fn build(self) -> std::io::Result<ConfigFs> {
        self.filesystem.unmount()?;

        Ok(ConfigFs { path: self.path })
    }
}

impl ConfigFs {
    pub fn builder(path: PathBuf, size: u64, label: &str) -> std::io::Result<ConfigFsBuilder> {
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

        Ok(ConfigFsBuilder { path, filesystem })
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
