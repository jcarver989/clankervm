use crate::util::sha256_hex;
use std::fs::{self, File};
use std::io::{self, Cursor};
use std::os::unix::fs::PermissionsExt;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

#[derive(Clone, Debug)]
pub struct Artifact {
    pub bytes: Vec<u8>,
    pub digest: String,
}

impl Artifact {
    /// Loads an existing ZIP file or creates a deterministic ZIP from a directory.
    pub fn load(path: &Path) -> io::Result<Self> {
        if path.is_dir() {
            Self::from_directory(path)
        } else {
            Self::from_zip(path)
        }
    }

    pub(crate) fn from_zip(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        // Validate the input now so an arbitrary file is not uploaded as a bundle.
        zip::ZipArchive::new(Cursor::new(&bytes)).map_err(zip_error)?;
        let digest = sha256_hex(&bytes);
        Ok(Self { bytes, digest })
    }

    fn from_directory(context: &Path) -> io::Result<Self> {
        let mut paths = Vec::new();
        collect(context, &mut paths)?;
        paths.sort();
        let mut output = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut output);
            for path in paths {
                let metadata = fs::symlink_metadata(&path)?;
                let relative = path.strip_prefix(context).expect("collected from context");
                let mut name = relative.to_string_lossy().replace(MAIN_SEPARATOR, "/");
                let options =
                    SimpleFileOptions::default().unix_permissions(metadata.permissions().mode());

                if metadata.is_symlink() {
                    let target = fs::read_link(&path)?;
                    zip.add_symlink(name, target.to_string_lossy(), options)
                        .map_err(zip_error)?;
                } else if metadata.is_dir() {
                    name.push('/');
                    zip.add_directory(name, options).map_err(zip_error)?;
                } else if metadata.is_file() {
                    zip.start_file(name, options).map_err(zip_error)?;
                    let mut file = File::open(path)?;
                    io::copy(&mut file, &mut zip)?;
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unsupported bundle context entry: {}", path.display()),
                    ));
                }
            }
            zip.finish().map_err(zip_error)?;
        }
        let bytes = output.into_inner();
        let digest = sha256_hex(&bytes);
        Ok(Self { bytes, digest })
    }
}

fn collect(directory: &Path, output: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        output.push(path.clone());
        if file_type.is_dir() {
            collect(&path, output)?;
        }
    }
    Ok(())
}

fn zip_error(error: zip::result::ZipError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn bundle_is_recursive_and_reproducible() {
        let directory = TempDir::new().unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        fs::create_dir(directory.path().join("empty")).unwrap();
        fs::write(directory.path().join("nested/file"), "content").unwrap();

        let first = Artifact::load(directory.path()).unwrap();
        let second = Artifact::load(directory.path()).unwrap();

        assert_eq!(first.digest, second.digest);
        assert_eq!(first.bytes, second.bytes);
        let mut archive = zip::ZipArchive::new(Cursor::new(first.bytes)).unwrap();
        assert!(archive.by_name("nested/file").is_ok());
        assert!(archive.by_name("empty/").is_ok());
    }

    #[test]
    fn existing_zip_is_loaded_without_repacking() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), "content").unwrap();
        let created = Artifact::load(&source).unwrap();
        let zip_path = directory.path().join("bundle.zip");
        fs::write(&zip_path, &created.bytes).unwrap();

        let loaded = Artifact::load(&zip_path).unwrap();

        assert_eq!(loaded.bytes, created.bytes);
        assert_eq!(loaded.digest, created.digest);
    }

    #[test]
    fn existing_non_zip_file_is_rejected() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("not-a-zip");
        fs::write(&path, "content").unwrap();

        assert_eq!(
            Artifact::load(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
