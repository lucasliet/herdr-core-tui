use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[cfg(windows)]
const PRIMARY_BINARY: &str = "herdr-core-tui.exe";
#[cfg(not(windows))]
const PRIMARY_BINARY: &str = "herdr-core-tui";

fn find_sibling_binary(exe_path: Option<&Path>) -> Option<PathBuf> {
    let dir = exe_path?.parent()?;
    let candidate = dir.join(PRIMARY_BINARY);
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Resolves the target `herdr-core-tui` executable path from sibling locations or PATH.
fn resolve_target_binary(current_exe: Option<&Path>, canonical_exe: Option<&Path>) -> OsString {
    if let Some(candidate) = find_sibling_binary(current_exe) {
        return candidate.into_os_string();
    }
    if let Some(candidate) = find_sibling_binary(canonical_exe) {
        return candidate.into_os_string();
    }
    OsString::from(PRIMARY_BINARY)
}

#[cfg(unix)]
fn exec_primary(target: &OsStr, args: Vec<OsString>) -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(target);
    command.args(args);
    command.exec()
}

#[cfg(not(unix))]
fn run_primary(target: &OsStr, args: Vec<OsString>) -> Result<ExitCode, std::io::Error> {
    let status = Command::new(target).args(args).status()?;
    let code = status.code().unwrap_or(1);
    let clamped = u8::try_from(code).unwrap_or(1);
    Ok(ExitCode::from(clamped))
}

fn main() -> ExitCode {
    let current_exe = env::current_exe().ok();
    let canonical_exe = current_exe
        .as_deref()
        .and_then(|path| fs::canonicalize(path).ok());
    let target = resolve_target_binary(current_exe.as_deref(), canonical_exe.as_deref());
    let args: Vec<OsString> = env::args_os().skip(1).collect();

    #[cfg(unix)]
    {
        let err = exec_primary(&target, args);
        eprintln!(
            "hc-tui: failed to execute `{}`: {err}",
            Path::new(&target).display()
        );
        ExitCode::from(1)
    }

    #[cfg(not(unix))]
    {
        match run_primary(&target, args) {
            Ok(code) => code,
            Err(err) => {
                eprintln!(
                    "hc-tui: failed to execute `{}`: {err}",
                    Path::new(&target).display()
                );
                ExitCode::from(1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path =
                env::temp_dir().join(format!("hc-tui-test-{name}-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp test dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_sibling_next_to_current_exe_first() {
        let current_dir = TempDir::new("current");
        let canonical_dir = TempDir::new("canonical");
        let current_primary = current_dir.0.join(PRIMARY_BINARY);
        let canonical_primary = canonical_dir.0.join(PRIMARY_BINARY);
        fs::write(&current_primary, b"").expect("write current sibling");
        fs::write(&canonical_primary, b"").expect("write canonical sibling");

        let current_exe = current_dir.0.join("hc-tui");
        let canonical_exe = canonical_dir.0.join("hc-tui");
        let resolved = resolve_target_binary(Some(&current_exe), Some(&canonical_exe));
        assert_eq!(resolved, current_primary.into_os_string());
    }

    #[test]
    fn falls_back_to_canonical_sibling_then_path_binary() {
        let current_dir = TempDir::new("symlink-dir");
        let canonical_dir = TempDir::new("real-dir");
        let canonical_primary = canonical_dir.0.join(PRIMARY_BINARY);
        fs::write(&canonical_primary, b"").expect("write canonical sibling");

        let current_exe = current_dir.0.join("hc-tui");
        let canonical_exe = canonical_dir.0.join("hc-tui");
        let resolved = resolve_target_binary(Some(&current_exe), Some(&canonical_exe));
        assert_eq!(resolved, canonical_primary.into_os_string());

        let bare = resolve_target_binary(Some(&current_exe), Some(&current_exe));
        assert_eq!(bare, OsString::from(PRIMARY_BINARY));
    }
}
