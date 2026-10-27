// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Secrets read from private files, so they stay out of shared environment files.
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::Path,
};
use zeroize::Zeroizing;

/// Reads a regular file that only its owner can read or write, owned by the
/// user this process runs as, at most `max_bytes` long. One trailing line
/// break is not part of the secret.
pub fn read(path: &Path, max_bytes: u64) -> io::Result<Zeroizing<String>> {
    read_as(path, max_bytes, process_user()?)
}

fn read_as(path: &Path, max_bytes: u64, user: u32) -> io::Result<Zeroizing<String>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Secret file must be a private regular file of this user within the size limit",
        )
    };
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() || before.mode() & 0o077 != 0 || before.uid() != user {
        return Err(invalid());
    }
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    // The path must still name the inspected file when it is opened.
    if before.dev() != opened.dev() || before.ino() != opened.ino() {
        return Err(invalid());
    }
    let mut text = Zeroizing::new(String::new());
    file.take(max_bytes + 1).read_to_string(&mut text)?;
    if text.len() as u64 > max_bytes {
        return Err(invalid());
    }
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

/// The effective user of this process: Linux makes it the owner of
/// `/proc/self` (proc(5)), which avoids `unsafe` FFI for `geteuid`. Without
/// procfs no secret can be read.
fn process_user() -> io::Result<u32> {
    Ok(fs::metadata("/proc/self")?.uid())
}

#[cfg(test)]
mod tests {
    use super::read;
    use std::os::unix::fs::PermissionsExt;

    fn private_file(name: &str, contents: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!("secret-file-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    #[test]
    fn private_files_are_read_without_their_trailing_line_break() {
        let path = private_file("secret", "  spaced secret  \r\n");
        assert_eq!(read(&path, 64).unwrap().as_str(), "  spaced secret  ");
        std::fs::write(&path, "multi\nline\n").unwrap();
        assert_eq!(read(&path, 64).unwrap().as_str(), "multi\nline");
    }

    #[test]
    fn shared_oversized_linked_and_missing_files_are_refused() {
        let path = private_file("secret", "value");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(read(&path, 64).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read(&path, 4).is_err());
        let link = path.with_file_name("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read(&link, 64).is_err());
        assert!(read(&path.with_file_name("missing"), 64).is_err());
        assert!(read(path.parent().unwrap(), 64).is_err());
    }

    #[test]
    fn a_private_file_of_another_user_is_refused() {
        use std::os::unix::fs::MetadataExt;
        let path = private_file("secret", "value");
        let owner = std::fs::metadata(&path).unwrap().uid();
        assert_eq!(super::process_user().unwrap(), owner);
        assert!(super::read_as(&path, 64, owner).is_ok());
        assert!(super::read_as(&path, 64, owner.wrapping_add(1)).is_err());
    }
}
