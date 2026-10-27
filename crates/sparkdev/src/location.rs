//! Compare retained state locations without changing the paths used for I/O.
//!
//! A packaged Windows process can reach the owner's directory through a
//! loopback administrative share while an ordinary process uses its drive
//! path. Textual UNC-to-drive replacement would bypass filesystem redirection.
use std::path::Path;

pub(crate) fn same_directory(left: &Path, right: &Path) -> bool {
    // Callers compare retained roots against an already validated canonical
    // Target.root. Preserve the exact old binding before probing alternate
    // spellings, including custom state on removable or mapped drives.
    if left == right {
        return true;
    }
    #[cfg(windows)]
    {
        windows::same(left, right, true)
    }
    #[cfg(not(windows))]
    {
        // Existing Unix profiles already retain canonical paths. Preserve the
        // original exact-path binding rather than adopting new aliases.
        left == right
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        fs,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        path::{Component, Prefix},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FileIdInfo, GetDriveTypeW, GetFileInformationByHandleEx,
    };

    /// Scope checks precede filesystem access. Never trust file identifiers
    /// supplied by an arbitrary SMB server or a mapped remote drive.
    pub(super) fn eligible(path: &Path) -> bool {
        if !path.is_absolute() {
            return false;
        }
        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return false;
        };
        let drive = match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                let (Some(server), Some(share)) = (server.to_str(), share.to_str()) else {
                    return false;
                };
                if !(server.eq_ignore_ascii_case("localhost") || server == "127.0.0.1")
                    || share.len() != 2
                    || !share.as_bytes()[0].is_ascii_alphabetic()
                    || share.as_bytes()[1] != b'$'
                {
                    return false;
                }
                share.as_bytes()[0]
            }
            _ => return false,
        };
        let root = [drive as u16, b':' as u16, b'\\' as u16, 0];
        // DRIVE_FIXED. This queries a local drive root, never the UNC server.
        unsafe { GetDriveTypeW(root.as_ptr()) == 3 }
    }

    fn ordinary(path: &Path, directory: bool) -> bool {
        for (index, part) in path.ancestors().enumerate() {
            let Ok(info) = fs::symlink_metadata(part) else {
                return false;
            };
            use std::os::windows::fs::MetadataExt;
            if info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                || info.file_type().is_symlink()
                || if index == 0 && !directory {
                    !info.is_file()
                } else {
                    !info.is_dir()
                }
            {
                return false;
            }
        }
        true
    }

    fn identity(path: &Path) -> Option<(u64, [u8; 16])> {
        let file = fs::OpenOptions::new()
            .read(true)
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .ok()?;
        let mut info = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
        // Both handles refer to the actual paths supplied by their caller;
        // no lexical drive/share conversion takes place before this query.
        let success = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if success == 0 {
            return None;
        }
        let info = unsafe { info.assume_init() };
        known_identity(info.VolumeSerialNumber, info.FileId.Identifier)
    }

    pub(super) fn known_identity(volume: u64, file_id: [u8; 16]) -> Option<(u64, [u8; 16])> {
        if volume == 0 || file_id == [0; 16] {
            None
        } else {
            Some((volume, file_id))
        }
    }

    pub(super) fn same(left: &Path, right: &Path, directory: bool) -> bool {
        if !eligible(left)
            || !eligible(right)
            || !ordinary(left, directory)
            || !ordinary(right, directory)
        {
            return false;
        }
        let (Ok(left_physical), Ok(right_physical)) =
            (fs::canonicalize(left), fs::canonicalize(right))
        else {
            return false;
        };
        if left_physical == right_physical {
            return true;
        }
        // A fixed local path and a literal loopback drive-share spelling may
        // name the same object. Unknown/unsupported identity is a refusal.
        match (identity(left), identity(right)) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        }
    }
}

#[cfg(all(test, windows))]
pub(crate) fn loopback_alias(path: &Path, host: &str) -> std::path::PathBuf {
    use std::path::{Component, Prefix};
    let physical = std::fs::canonicalize(path).unwrap();
    let mut components = physical.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        panic!("The Windows fixture needs a local drive");
    };
    let drive = match prefix.kind() {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
        _ => panic!("The Windows fixture needs a local drive"),
    };
    assert!(matches!(components.next(), Some(Component::RootDir)));
    let mut alias = std::path::PathBuf::from(format!("\\\\{host}\\{}$\\", drive as char));
    alias.push(components.as_path());
    alias
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_retained_binding_needs_no_new_filesystem_probe() {
        // The caller already validated this canonical path. Exact equality
        // must not introduce a new fixed-drive or network-access requirement.
        let retained = Path::new(r"Z:\validated-custom-state\nodes\retained");
        assert!(same_directory(retained, retained));
    }

    #[cfg(windows)]
    #[test]
    fn zero_windows_file_or_volume_identifiers_are_unknown() {
        assert!(windows::known_identity(0, [1; 16]).is_none());
        assert!(windows::known_identity(1, [0; 16]).is_none());
        assert!(windows::known_identity(0, [0; 16]).is_none());
        assert_eq!(windows::known_identity(1, [2; 16]), Some((1, [2; 16])));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_keeps_exact_retained_path_binding() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        assert!(same_directory(root.path(), root.path()));
        assert!(!same_directory(root.path(), other.path()));
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(root.path(), &alias).unwrap();
        assert!(!same_directory(root.path(), &alias));
    }

    #[cfg(windows)]
    #[test]
    fn foreign_unc_and_nonadministrative_shares_are_refused_before_io() {
        for path in [
            r"\\example.invalid\C$\owner",
            r"\\?\UNC\example.invalid\C$\owner",
            r"\\localhost\ordinary-share\owner",
            r"\\127.0.0.1.evil.invalid\C$\owner",
            r"\\.\C:\owner",
            r"C:relative",
        ] {
            assert!(!windows::eligible(Path::new(path)), "{path}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn real_windows_loopback_aliases_match_only_the_same_directory_or_file() {
        let profile = std::env::var_os("USERPROFILE").unwrap();
        let root = tempfile::tempdir_in(profile).unwrap();
        let alias = loopback_alias(root.path(), "localhost");
        if !alias.is_dir() {
            eprintln!(
                "Local administrative share unavailable; physical alias fixture not exercised"
            );
            return;
        }
        assert!(same_directory(root.path(), &alias));
        assert!(same_directory(&alias, root.path()));
        let other = root.path().join("different-directory");
        std::fs::create_dir(&other).unwrap();
        assert!(!same_directory(
            root.path(),
            &loopback_alias(&other, "localhost")
        ));
        let file = root.path().join("worker.exe");
        let copy = root.path().join("debug-copy.exe");
        std::fs::write(&file, b"same bytes do not establish executable identity").unwrap();
        std::fs::copy(&file, &copy).unwrap();
        assert!(windows::same(
            &file,
            &loopback_alias(&file, "localhost"),
            false
        ));
        assert!(!windows::same(
            &file,
            &loopback_alias(&copy, "localhost"),
            false
        ));
        assert!(!same_directory(&file, &loopback_alias(&file, "localhost")));
        assert!(!same_directory(&root.path().join("missing"), &alias));
        let ip_alias = loopback_alias(root.path(), "127.0.0.1");
        if ip_alias.is_dir() {
            assert!(same_directory(root.path(), &ip_alias));
        }
        let linked = root.path().join("linked-directory");
        if std::os::windows::fs::symlink_dir(root.path(), &linked).is_ok() {
            assert!(!same_directory(&linked, &alias));
        }
    }
}
