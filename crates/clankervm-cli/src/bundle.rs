use base16ct::lower::encode_string;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Cursor};
use std::os::unix::fs::PermissionsExt;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZipBundle {
    #[serde(skip)]
    pub bytes: Vec<u8>,
    pub digest: String,
}

impl ZipBundle {
    pub fn from_path(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        let digest = encode_string(Sha256::digest(&bytes).as_ref());
        Ok(Self { bytes, digest })
    }
}

pub(crate) fn create_zip_bundle(context: &Path) -> io::Result<ZipBundle> {
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
            let mut options = SimpleFileOptions::default();
            options = options.unix_permissions(metadata.permissions().mode());

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
    let digest = encode_string(Sha256::digest(&bytes).as_ref());
    Ok(ZipBundle { bytes, digest })
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

        let first = create_zip_bundle(directory.path()).unwrap();
        let second = create_zip_bundle(directory.path()).unwrap();

        assert_eq!(first.digest, second.digest);
        assert_eq!(first.bytes, second.bytes);
        let mut archive = zip::ZipArchive::new(Cursor::new(first.bytes)).unwrap();
        assert!(archive.by_name("nested/file").is_ok());
        assert!(archive.by_name("empty/").is_ok());
    }
}
