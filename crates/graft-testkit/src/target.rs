//! Find the cargo profile directory (`target/<profile>`) that holds the
//! workspace binaries, from a test or bench executable.
//!
//! Cargo puts test executables in `target/<profile>/deps/<exe>` (the old
//! layout) or `target/<profile>/build/<pkg>/<hash>/out/<exe>` (the build-dir
//! layout, the default since Rust 1.100). Binaries such as `graft` and
//! `graft-server` are still uplifted to `target/<profile>/` in both.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The profile directory for `exe`. An executable in neither test layout
/// (a binary run directly) is taken to sit in the profile directory itself.
#[must_use]
pub fn profile_dir_of(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    if dir.file_name() == Some(OsStr::new("deps")) {
        return dir.parent().map(Path::to_path_buf);
    }
    if dir.file_name() == Some(OsStr::new("out")) {
        let build = dir.parent()?.parent()?.parent()?;
        if build.file_name() == Some(OsStr::new("build")) {
            return build.parent().map(Path::to_path_buf);
        }
    }
    Some(dir.to_path_buf())
}

/// The profile directory for the running executable.
///
/// # Panics
///
/// When the current executable's path cannot be read.
#[must_use]
pub fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current test executable");
    profile_dir_of(&exe).expect("workspace target profile directory")
}

#[cfg(test)]
mod tests {
    use super::profile_dir_of;
    use std::path::Path;

    #[test]
    fn deps_layout() {
        assert_eq!(
            profile_dir_of(Path::new("/w/target/debug/deps/api-0123abcd")).unwrap(),
            Path::new("/w/target/debug")
        );
    }

    #[test]
    fn build_dir_layout() {
        assert_eq!(
            profile_dir_of(Path::new(
                "/w/target/debug/build/graft-cli/45f38afc12c1acb9/out/unified_roles-45f38afc12c1acb9"
            ))
            .unwrap(),
            Path::new("/w/target/debug")
        );
    }

    #[test]
    fn a_binary_in_the_profile_dir() {
        assert_eq!(
            profile_dir_of(Path::new("/build/w/target/release/graft-pilot")).unwrap(),
            Path::new("/build/w/target/release")
        );
    }

    #[test]
    fn the_running_test_finds_its_profile_dir() {
        let dir = super::profile_dir();
        assert!(
            dir.join("build").is_dir() || dir.join("deps").is_dir(),
            "{} is not a cargo profile directory",
            dir.display()
        );
    }
}
