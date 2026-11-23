use std::{ffi::OsStr, path::Path};

use anyhow::{Context, Result};
use tokio::process::Command;

pub(super) fn command(
    workspace: &Path,
    program: impl AsRef<OsStr>,
    writable: bool,
) -> Result<Command> {
    let workspace = workspace
        .canonicalize()
        .context("resolve workspace sandbox root")?;
    let mut command = platform_command(&workspace, program.as_ref(), writable)?;
    command
        .current_dir(&workspace)
        .env("TMPDIR", &workspace)
        // Cargo takes cache locks and writes registry state even for ordinary builds.
        .env("CARGO_HOME", workspace.join(".ratio/cargo-cache"))
        // A host cache daemon can write on behalf of its clients outside this sandbox.
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    Ok(command)
}

#[cfg(target_os = "macos")]
fn platform_command(workspace: &Path, program: &OsStr, writable: bool) -> Result<Command> {
    use std::ffi::OsString;

    let mut parameter = OsString::from("WORKSPACE=");
    parameter.push(workspace);
    let mut profile = String::from(
        r#"(version 1)
(allow default)
(deny file-write*)
(allow file-write-data (literal "/dev/null"))
"#,
    );
    if writable {
        profile.push_str("(allow file-write* (subpath (param \"WORKSPACE\")))");
    }
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.arg("-D").arg(parameter);
    command.arg("-p").arg(profile).arg("--").arg(program);
    Ok(command)
}

#[cfg(target_os = "linux")]
fn platform_command(workspace: &Path, program: &OsStr, writable: bool) -> Result<Command> {
    use anyhow::Context;
    use std::os::fd::AsRawFd;

    let rules = linux::workspace_rules(workspace, writable)
        .context("workspace sandbox requires Linux Landlock ABI 3 or later")?;
    let mut command = Command::new(program);
    // The child is single-threaded here. Enforcement uses syscalls only and
    // the ruleset descriptor is closed automatically on exec.
    unsafe { command.pre_exec(move || linux::enforce(rules.as_raw_fd())) };
    Ok(command)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_command(_: &Path, _: &OsStr, _: bool) -> Result<Command> {
    anyhow::bail!("workspace sandbox unavailable on this platform")
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        ffi::CString,
        io,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
            unix::ffi::OsStrExt,
        },
        path::Path,
    };

    use anyhow::{Result, ensure};

    const WRITE_FILE: u64 = 1 << 1;
    const FIRST_DIRECTORY_MUTATION: u64 = 1 << 4;
    const TRUNCATE: u64 = 1 << 14;
    // Write and directory-entry mutations, including REFER (ABI 2) and TRUNCATE (ABI 3).
    const HANDLED: u64 = WRITE_FILE | (TRUNCATE * 2 - FIRST_DIRECTORY_MUTATION);

    #[repr(C, packed)]
    struct PathRule {
        allowed_access: u64,
        parent_fd: RawFd,
    }

    pub(super) fn workspace_rules(workspace: &Path, writable: bool) -> Result<OwnedFd> {
        let version = syscall_result(unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<u64>(),
                0usize,
                1u32, // LANDLOCK_CREATE_RULESET_VERSION
            )
        })?;
        ensure!(version >= 3, "kernel reports Landlock ABI {version}");
        let fd = syscall_result(unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &HANDLED as *const u64,
                size_of::<u64>(),
                0u32,
            )
        })?;
        // A successful create_ruleset returns a new close-on-exec descriptor.
        let rules = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        if writable {
            add_path(&rules, workspace, HANDLED)?;
        }
        add_path(&rules, Path::new("/dev/null"), WRITE_FILE)?;
        Ok(rules)
    }

    fn add_path(rules: &OwnedFd, path: &Path, access: u64) -> io::Result<()> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        // O_PATH pins the resolved inode without opening it for data access.
        let fd = syscall_result(
            unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) }.into(),
        )?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        let attr = PathRule {
            allowed_access: access,
            parent_fd: fd.as_raw_fd(),
        };
        syscall_result(unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                rules.as_raw_fd(),
                1u32, // LANDLOCK_RULE_PATH_BENEATH
                &attr as *const PathRule,
                0u32,
            )
        })?;
        Ok(())
    }

    pub(super) fn enforce(rules: RawFd) -> io::Result<()> {
        syscall_result(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }.into())?;
        syscall_result(unsafe { libc::syscall(libc::SYS_landlock_restrict_self, rules, 0u32) })?;
        Ok(())
    }

    fn syscall_result(status: libc::c_long) -> io::Result<libc::c_long> {
        if status < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(status)
        }
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use std::{fs, os::unix::fs::symlink};

    use super::*;

    #[tokio::test]
    async fn subprocesses_allow_external_reads_and_confine_writes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace with \"quotes\"");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("inside"), "workspace data").unwrap();
        fs::write(temp.path().join("outside"), "outside data").unwrap();
        symlink(temp.path(), workspace.join("link")).unwrap();
        // Launch a child command to exercise inherited sandbox policy.
        fs::write(
            workspace.join("check.sh"),
            r#"set -eu
cat inside
cat ../outside link/outside
if touch ../created || touch link/created; then exit 2; fi
if /bin/sh -c 'printf overwritten > link/outside'; then exit 3; fi
if touch generated; then printf 'writable\n'; fi
"#,
        )
        .unwrap();
        for writable in [false, true] {
            let output = command(&workspace, "/bin/sh", writable)
                .unwrap()
                .args(["-c", "/bin/sh check.sh"])
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let expected = if writable {
                "workspace dataoutside dataoutside datawritable\n"
            } else {
                "workspace dataoutside dataoutside data"
            };
            assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
            assert_eq!(workspace.join("generated").exists(), writable);
            assert!(!temp.path().join("created").exists());
            assert_eq!(
                fs::read_to_string(temp.path().join("outside")).unwrap(),
                "outside data"
            );
        }
    }

    #[tokio::test]
    async fn shell_can_build_with_the_installed_rust_toolchain() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'sandbox-probe'\nversion = '0.1.0'\nedition = '2021'\n[workspace]\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "#[test] fn compiled() { assert_eq!(2 + 2, 4); }",
        )
        .unwrap();
        let output = command(root, "/bin/sh", true)
            .unwrap()
            .args([
                "-ec",
                "rustc --version; cargo test --offline --target-dir target",
            ])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert!(root.join(".ratio/cargo-cache").is_dir());
        assert!(root.join("target").is_dir());
    }
}
