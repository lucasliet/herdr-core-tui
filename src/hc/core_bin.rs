use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

static BIN: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn path() -> PathBuf {
    BIN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| PathBuf::from("hc"))
}

pub fn use_core(bin: PathBuf) {
    let path = std::env::var_os("PATH").unwrap_or_default();
    if let Some(joined) = path_for_core(&path, &bin, &hc_names()) {
        unsafe {
            std::env::set_var("PATH", joined);
        }
    }
    *BIN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(bin);
}

fn path_for_core(
    path: &std::ffi::OsStr,
    bin: &Path,
    hc_names: &[String],
) -> Option<std::ffi::OsString> {
    let dir = bin.parent()?;
    let dirs: Vec<PathBuf> = if path.is_empty() {
        Vec::new()
    } else {
        std::env::split_paths(path).collect()
    };
    let hcs_in = |d: &Path| {
        hc_names
            .iter()
            .map(|name| d.join(name))
            .filter(|hc| hc.is_file())
            .collect::<Vec<_>>()
    };
    let stop = dirs
        .iter()
        .position(|d| d.is_relative() || !hcs_in(d).is_empty());
    if let Some(d) = stop.map(|i| &dirs[i]) {
        if d.is_absolute()
            && (d == dir
                || bin.canonicalize().is_ok_and(|core| {
                    hcs_in(d)
                        .iter()
                        .all(|hc| hc.canonicalize().is_ok_and(|hc| hc == core))
                }))
        {
            return None;
        }
    }
    let (before, after) = dirs.split_at(stop.unwrap_or(0));
    let others = |d: &&PathBuf| *d != dir;
    let dirs = before
        .iter()
        .filter(others)
        .map(PathBuf::as_path)
        .chain([dir])
        .chain(after.iter().filter(others).map(PathBuf::as_path));
    std::env::join_paths(dirs).ok()
}

fn hc_names() -> Vec<String> {
    if cfg!(windows) {
        pathext_names(&std::env::var("PATHEXT").unwrap_or_default())
    } else {
        vec!["hc".into()]
    }
}

fn pathext_names(pathext: &str) -> Vec<String> {
    let pathext = if pathext.trim().is_empty() {
        ".COM;.EXE;.BAT;.CMD"
    } else {
        pathext
    };
    let exts = pathext
        .split(';')
        .map(str::trim)
        .filter(|ext| !ext.is_empty());
    exts.chain([".ps1"])
        .map(|ext| format!("hc{}", ext.to_ascii_lowercase()))
        .collect()
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn version(bin: &Path) -> Option<String> {
    let out = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && text.starts_with("hc ")).then_some(text)
}

pub fn find(configured: Option<&str>) -> Option<PathBuf> {
    let home = home_dir();
    let pick = |candidates: Vec<PathBuf>| {
        candidates
            .into_iter()
            .filter(|c| c.is_file())
            .find(|c| version(c).is_some())
    };
    if let Some(env_bin) = std::env::var("HC_BIN")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        if let Some(found) = pick(vec![expand(&env_bin, &home)]) {
            return Some(found);
        }
    }
    if let Some(found) = pick(
        configured
            .map(|c| vec![expand(c, &home)])
            .unwrap_or_default(),
    ) {
        return Some(found);
    }
    if let Some(dirs) = std::env::var_os("HERDR_CORE_DIRS") {
        return pick(
            std::env::split_paths(&dirs)
                .flat_map(|d| names(&d))
                .collect(),
        );
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let quick: Vec<PathBuf> = dirs_on(&path)
        .chain(usual_dirs(&home))
        .flat_map(|d| names(&d))
        .collect();
    pick(quick).or_else(|| {
        pick(
            dirs_on(&login_shell_path()?)
                .flat_map(|d| names(&d))
                .collect(),
        )
    })
}

fn expand(p: &str, home: &Path) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(p),
    }
}

fn names(dir: &Path) -> [PathBuf; 2] {
    let exe = std::env::consts::EXE_SUFFIX;
    [
        dir.join(format!("herdr-core{exe}")),
        dir.join(format!("hc{exe}")),
    ]
}

fn dirs_on(path: &std::ffi::OsStr) -> impl Iterator<Item = PathBuf> + '_ {
    std::env::split_paths(path)
}

fn usual_dirs(home: &Path) -> Vec<PathBuf> {
    if cfg!(windows) {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Local"));
        return vec![local.join("Programs/herdr-core"), home.join(".cargo/bin")];
    }
    let mut dirs = vec![
        home.join(".local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".local/share/mise/shims"),
        home.join(".nix-profile/bin"),
        PathBuf::from("/run/current-system/sw/bin"),
        home.join(".cargo/bin"),
    ];
    if cfg!(target_os = "macos") {
        dirs.push(home.join(".local/share/herdr-core/current/Contents/MacOS"));
        dirs.push(home.join(".local/share/herdr-core/Herdr Core.app/Contents/MacOS"));
    }
    dirs
}

fn login_shell_path() -> Option<std::ffi::OsString> {
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut child = Command::new(shell)
        .args(["-lc", "printf 'herdr-path[%s]herdr-path' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if child.try_wait().ok()?.is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    login_path_answer(&out)
}

fn login_path_answer(out: &str) -> Option<std::ffi::OsString> {
    let (_, rest) = out.split_once("herdr-path[")?;
    let (path, _) = rest.rsplit_once("]herdr-path")?;
    (!path.is_empty() && path != "$PATH").then(|| path.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_path_answer_extracts_framed_path() {
        let output = "banner line\nherdr-path[/usr/local/bin:/opt/homebrew/bin]herdr-path\n";
        assert_eq!(
            login_path_answer(output),
            Some(std::ffi::OsString::from("/usr/local/bin:/opt/homebrew/bin"))
        );
        assert_eq!(login_path_answer("herdr-path[$PATH]herdr-path"), None);
    }

    #[test]
    fn path_for_core_prepends_directory_before_competing_hc() {
        let temp = std::env::temp_dir().join(format!("herdr-core-bin-test-{}", std::process::id()));
        let first = temp.join("first");
        let second = temp.join("second");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let fake_other = first.join("hc");
        let fake_target = second.join("hc");
        std::fs::write(&fake_other, "").unwrap();
        std::fs::write(&fake_target, "").unwrap();
        let raw_path = std::env::join_paths([&first]).unwrap();
        let updated = path_for_core(&raw_path, &fake_target, &["hc".into()]).unwrap();
        let joined: Vec<PathBuf> = std::env::split_paths(&updated).collect();
        assert_eq!(joined, vec![second, first]);
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[cfg(unix)]
    #[test]
    fn find_returns_configured_executable_file_without_installing() {
        use std::os::unix::fs::PermissionsExt;

        let temp =
            std::env::temp_dir().join(format!("herdr-core-find-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let bin = temp.join("hc");
        std::fs::write(&bin, "#!/bin/sh\necho \"hc 0.1.21\"\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let missing = temp.join("missing-hc");

        assert_eq!(find(Some(bin.to_string_lossy().as_ref())), Some(bin));
        assert!(find(Some(missing.to_string_lossy().as_ref())).is_none() || find(None).is_some());

        let _ = std::fs::remove_dir_all(&temp);
    }
}
