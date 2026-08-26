use aws_sdk_lambdamicrovms::types::{HookState, Hooks, MicrovmHooks, MicrovmImageHooks};
use base16ct::lower::encode_string;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Cursor};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

pub struct ContextArtifact {
    pub bytes: Vec<u8>,
    pub digest: String,
}

pub fn zip_context(context: &Path) -> io::Result<ContextArtifact> {
    let mut paths = Vec::new();
    collect(context, &mut paths)?;
    paths.sort();
    let mut output = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut output);
        for path in paths {
            let metadata = fs::symlink_metadata(&path)?;
            let relative = path.strip_prefix(context).expect("collected from context");
            let mut name = relative
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let mut options = SimpleFileOptions::default();
            #[cfg(unix)]
            {
                options = options.unix_permissions(metadata.permissions().mode());
            }

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
                    format!("unsupported build context entry: {}", path.display()),
                ));
            }
        }
        zip.finish().map_err(zip_error)?;
    }
    let bytes = output.into_inner();
    let digest = encode_string(Sha256::digest(&bytes).as_ref());
    Ok(ContextArtifact { bytes, digest })
}

pub fn hooks(port: i32, ready_timeout: i32, run_timeout: i32, terminate_timeout: i32) -> Hooks {
    Hooks::builder()
        .port(port)
        .microvm_image_hooks(
            MicrovmImageHooks::builder()
                .ready(HookState::Enabled)
                .ready_timeout_in_seconds(ready_timeout)
                .build(),
        )
        .microvm_hooks(
            MicrovmHooks::builder()
                .run(HookState::Enabled)
                .run_timeout_in_seconds(run_timeout)
                .terminate(HookState::Enabled)
                .terminate_timeout_in_seconds(terminate_timeout)
                .build(),
        )
        .build()
}

fn zip_error(error: zip::result::ZipError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
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
