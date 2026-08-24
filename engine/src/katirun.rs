//! Sarun adapters for Bumba's embedded Make implementation.

use std::os::unix::ffi::OsStrExt as _;
use std::sync::Arc;

use brush_core::vfs::BoxVfs as _;

pub use bumba::make::{
    cap_text, extract_var_refs, is_make_invocation, push_makevar, vartrace_enabled,
};

pub fn start_activity_reporting() {
    crate::bumba_adapter::install();
    bumba::make::start_activity_reporting();
}

struct DirectKatiFileSystem {
    client: Arc<crate::direct_fs::DirectFsClient>,
    cwd: std::path::PathBuf,
}

impl DirectKatiFileSystem {
    fn new(
        client: Arc<crate::direct_fs::DirectFsClient>,
        cwd: std::path::PathBuf,
    ) -> std::io::Result<Self> {
        if !cwd.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Kati direct filesystem cwd must be absolute",
            ));
        }
        Ok(Self { client, cwd })
    }

    fn absolute(&self, path: &std::path::Path) -> std::path::PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    fn mode_kind(mode: u32) -> bumba::FileKind {
        match mode & libc::S_IFMT as u32 {
            value if value == libc::S_IFREG as u32 => bumba::FileKind::Regular,
            value if value == libc::S_IFDIR as u32 => bumba::FileKind::Directory,
            value if value == libc::S_IFLNK as u32 => bumba::FileKind::Symlink,
            value if value == libc::S_IFIFO as u32 => bumba::FileKind::Fifo,
            value if value == libc::S_IFSOCK as u32 => bumba::FileKind::Socket,
            value if value == libc::S_IFCHR as u32 => bumba::FileKind::CharDevice,
            value if value == libc::S_IFBLK as u32 => bumba::FileKind::BlockDevice,
            _ => bumba::FileKind::Unknown,
        }
    }

    fn dirent_kind(file_type: u32) -> bumba::FileKind {
        match file_type {
            value if value == libc::DT_REG as u32 => bumba::FileKind::Regular,
            value if value == libc::DT_DIR as u32 => bumba::FileKind::Directory,
            value if value == libc::DT_LNK as u32 => bumba::FileKind::Symlink,
            value if value == libc::DT_FIFO as u32 => bumba::FileKind::Fifo,
            value if value == libc::DT_SOCK as u32 => bumba::FileKind::Socket,
            value if value == libc::DT_CHR as u32 => bumba::FileKind::CharDevice,
            value if value == libc::DT_BLK as u32 => bumba::FileKind::BlockDevice,
            _ => bumba::FileKind::Unknown,
        }
    }

    fn metadata(&self, metadata: crate::direct_fs::DirectFsMetadata) -> bumba::Metadata {
        bumba::Metadata {
            kind: Self::mode_kind(metadata.mode),
            len: metadata.len,
            modified: metadata.modified,
        }
    }
}

impl bumba::FileSystemProvider for DirectKatiFileSystem {
    fn metadata(&self, path: &std::path::Path) -> std::io::Result<bumba::Metadata> {
        self.client
            .direct_metadata(&self.absolute(path))
            .map(|metadata| self.metadata(metadata))
    }

    fn symlink_metadata(&self, path: &std::path::Path) -> std::io::Result<bumba::Metadata> {
        self.client
            .direct_symlink_metadata(&self.absolute(path))
            .map(|metadata| self.metadata(metadata))
    }

    fn modified(&self, path: &std::path::Path) -> std::io::Result<std::time::SystemTime> {
        self.client
            .direct_metadata(&self.absolute(path))
            .map(|metadata| metadata.modified)
    }

    fn read(&self, path: &std::path::Path) -> std::io::Result<Vec<u8>> {
        self.client.read(&self.absolute(path))
    }

    fn read_link(&self, path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
        self.client.read_link(&self.absolute(path))
    }

    fn read_dir(
        &self,
        path: &std::path::Path,
    ) -> std::io::Result<Vec<std::io::Result<bumba::DirEntry>>> {
        self.client.direct_read_dir(&self.absolute(path)).map(|entries| {
            entries
                .into_iter()
                .map(|entry| {
                    let mut kind = Self::dirent_kind(entry.file_type);
                    if kind == bumba::FileKind::Unknown {
                        kind = Self::mode_kind(
                            self.client.direct_symlink_metadata(&entry.path)?.mode,
                        );
                    }
                    Ok(bumba::DirEntry {
                        file_name: entry.file_name,
                        path: entry.path,
                        kind,
                    })
                })
                .collect()
        })
    }

    fn canonicalize(&self, path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
        self.client.canonicalize(&self.absolute(path))
    }

    fn glob(&self, pattern: &[u8]) -> std::io::Result<Vec<Vec<u8>>> {
        let pattern = std::path::Path::new(std::ffi::OsStr::from_bytes(pattern));
        let absolute = self.absolute(pattern);
        self.client.glob(absolute.as_os_str().as_bytes())
    }
}

pub fn make_main(argv: &[String]) -> i32 {
    crate::bumba_adapter::install();
    bumba::make::make_main(argv)
}

pub fn make_builtin(
    argv: &[String],
    base_cwd: &std::path::Path,
    seed_env: &[(std::ffi::OsString, std::ffi::OsString)],
    out: impl std::io::Write,
    mut err: impl std::io::Write,
    recipe_out: Box<dyn std::io::Write>,
    recipe_err: Box<dyn std::io::Write>,
    stdin: Option<brush_core::openfiles::OpenFile>,
) -> i32 {
    crate::bumba_adapter::install();
    let filesystem = match crate::direct_fs::current() {
        Some(client) => match DirectKatiFileSystem::new(client, base_cwd.to_path_buf()) {
            Ok(provider) => Some(Arc::new(provider) as Arc<dyn bumba::FileSystemProvider>),
            Err(error) => {
                let _ = writeln!(err, "sarun-engine make: filesystem adapter: {error}");
                return 2;
            }
        },
        None => {
            let _ = writeln!(
                err,
                "sarun-engine make: direct filesystem capability unavailable"
            );
            return 2;
        }
    };
    bumba::make::make_builtin(
        argv,
        base_cwd,
        seed_env,
        out,
        err,
        recipe_out,
        recipe_err,
        stdin,
        filesystem,
    )
}
