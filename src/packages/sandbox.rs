//! Confinement for package version checks.
//!
//! A version check executes a file from the package. For an imported GitHub
//! package that is code written by a third party, and it runs as the RAST
//! service account, which can read the RAST database and reach the Docker
//! worker socket. Before `exec`, the check process is confined:
//!
//! * **Process group** — the check runs in its own process group, which is
//!   killed as a whole on timeout and again after it exits; `setsid` and
//!   `setpgid` are denied, so no descendant can leave the group and linger.
//! * **seccomp** — no new sockets (no network, no connection to the worker or
//!   any other local service), no io_uring, no `ptrace`/`process_vm_*`, and
//!   only the native syscall ABI.
//! * **Landlock** (when the kernel provides it; required for imported
//!   packages) — read/execute only inside the package directory, the system
//!   binary/library directories, a few name-service files under `/etc` and
//!   the basic `/dev` nodes; write only inside a private scratch home. TCP
//!   bind/connect is denied (ABI 4+), as are abstract Unix sockets and
//!   signals to processes outside the sandbox (ABI 6+).
//! * **Resource limits** — no core dumps; files are capped at 64 MiB.
//!
//! Output goes to files rather than pipes, so a descendant holding a pipe
//! open can never block the job; the wall-clock timeout covers the rest.

use std::{
    fs, io,
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{fs::OpenOptionsExt, process::CommandExt},
    },
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

const FILE_SIZE_LIMIT: u64 = 64 * 1024 * 1024;

// Landlock UAPI (linux/landlock.h).
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;
const FS_IOCTL_DEV: u64 = 1 << 15;
/// Rights that apply to files (the others only make sense on directories).
const FS_FILE_RIGHTS: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;
const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// System directories a runtime may read and execute from.
const SYSTEM_DIRS: &[&str] = &[
    "/usr", "/lib", "/lib64", "/lib32", "/libx32", "/bin", "/sbin",
];
/// Read-only files and directories for the dynamic loader, name service,
/// time zone and TLS roots. Nothing else under `/etc` (RAST's configuration
/// lives there) is readable.
const SYSTEM_READ: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/ld.so.preload",
    "/etc/localtime",
    "/etc/timezone",
    "/etc/nsswitch.conf",
    "/etc/passwd",
    "/etc/group",
    "/etc/hosts",
    "/etc/host.conf",
    "/etc/resolv.conf",
    "/etc/gai.conf",
    "/etc/os-release",
    "/etc/mime.types",
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/alternatives",
    "/proc/meminfo",
    "/proc/cpuinfo",
    "/proc/stat",
    "/sys/devices/system/cpu",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
];

/// The Landlock ABI version of the running kernel, or `None` when Landlock is
/// not available (not built, not enabled at boot, or blocked).
pub fn landlock_abi() -> Option<i32> {
    // SAFETY: querying the ABI version takes no pointers.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    (abi >= 1).then_some(abi as i32)
}

/// Why imported packages cannot be installed on this host, if they cannot.
pub fn imported_unsupported_reason() -> Option<String> {
    landlock_abi().is_none().then(|| {
        "This server's kernel does not provide Landlock, which RAST needs to confine the \
         version checks of imported packages. Official catalog packages still install."
            .to_string()
    })
}

/// A prepared confinement: build it in the parent, then apply it to one
/// command. Everything that allocates happens here; the hook that runs in the
/// forked child only makes system calls.
pub struct Confinement {
    ruleset: Option<OwnedFd>,
    filter: Arc<Vec<libc::sock_filter>>,
    pub landlock_abi: Option<i32>,
}

impl Confinement {
    /// `root` is the package directory (read/execute), `home` the scratch
    /// directory the check may write to. With `require_landlock`, a kernel
    /// without Landlock is an error instead of a weaker sandbox.
    pub fn new(root: &Path, home: &Path, require_landlock: bool) -> Result<Self, String> {
        let filter = Arc::new(seccomp_filter()?);
        let abi = landlock_abi();
        let ruleset = match abi {
            Some(abi) => Some(
                landlock_ruleset(abi, root, home)
                    .map_err(|e| format!("cannot prepare the Landlock sandbox: {e}"))?,
            ),
            None if require_landlock => {
                return Err(imported_unsupported_reason().unwrap_or_default());
            }
            None => None,
        };
        Ok(Self {
            ruleset,
            filter,
            landlock_abi: abi,
        })
    }

    /// Install the confinement on `cmd` (own process group + pre-exec hook).
    pub fn apply(&self, cmd: &mut Command) {
        let ruleset: Option<RawFd> = self.ruleset.as_ref().map(|fd| fd.as_raw_fd());
        let filter = self.filter.clone();
        cmd.process_group(0);
        // SAFETY: the hook only calls async-signal-safe system calls on data
        // prepared before the fork (the ruleset fd and the BPF program).
        unsafe {
            cmd.pre_exec(move || confine_self(ruleset, &filter));
        }
    }
}

fn confine_self(ruleset: Option<RawFd>, filter: &[libc::sock_filter]) -> io::Result<()> {
    let core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let fsize = libc::rlimit {
        rlim_cur: FILE_SIZE_LIMIT as libc::rlim_t,
        rlim_max: FILE_SIZE_LIMIT as libc::rlim_t,
    };
    // SAFETY: plain system calls with valid pointers to stack values.
    unsafe {
        if libc::setrlimit(libc::RLIMIT_CORE, &core) != 0
            || libc::setrlimit(libc::RLIMIT_FSIZE, &fsize) != 0
        {
            return Err(io::Error::last_os_error());
        }
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        if let Some(fd) = ruleset {
            if libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let prog = libc::sock_fprog {
            len: filter.len() as libc::c_ushort,
            filter: filter.as_ptr() as *mut libc::sock_filter,
        };
        if libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &prog as *const libc::sock_fprog,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn landlock_ruleset(abi: i32, root: &Path, home: &Path) -> io::Result<OwnedFd> {
    let mut handled_fs = (1u64 << 13) - 1; // ABI 1: execute .. make_sym
    if abi >= 2 {
        handled_fs |= FS_REFER;
    }
    if abi >= 3 {
        handled_fs |= FS_TRUNCATE;
    }
    if abi >= 5 {
        handled_fs |= FS_IOCTL_DEV;
    }
    let attr = RulesetAttr {
        handled_access_fs: handled_fs,
        handled_access_net: if abi >= 4 {
            NET_BIND_TCP | NET_CONNECT_TCP
        } else {
            0
        },
        scoped: if abi >= 6 {
            SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
        } else {
            0
        },
    };
    // Older kernels reject fields they do not know: pass only the known size.
    let size = match abi {
        1..=3 => 8,
        4 | 5 => 16,
        _ => std::mem::size_of::<RulesetAttr>(),
    };
    // SAFETY: `attr` is a valid ruleset attribute of at least `size` bytes.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            size,
            0u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel returned a new file descriptor (close-on-exec).
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    let read_exec = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;
    allow(&ruleset, root, read_exec, handled_fs, true)?;
    allow(&ruleset, home, handled_fs, handled_fs, true)?;
    for dir in SYSTEM_DIRS {
        allow(&ruleset, Path::new(dir), read_exec, handled_fs, false)?;
    }
    for path in SYSTEM_READ {
        allow(
            &ruleset,
            Path::new(path),
            FS_READ_FILE | FS_READ_DIR,
            handled_fs,
            false,
        )?;
    }
    allow(
        &ruleset,
        Path::new("/dev/null"),
        FS_READ_FILE | FS_WRITE_FILE,
        handled_fs,
        false,
    )?;
    Ok(ruleset)
}

/// Add a path-beneath rule. Missing optional paths are skipped; rights that
/// only apply to directories are dropped for files.
fn allow(
    ruleset: &OwnedFd,
    path: &Path,
    access: u64,
    handled: u64,
    required: bool,
) -> io::Result<()> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if !required && e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    let is_dir = file.metadata()?.is_dir();
    let mut allowed = access & handled;
    if !is_dir {
        allowed &= FS_FILE_RIGHTS;
    }
    let attr = PathBeneathAttr {
        allowed_access: allowed,
        parent_fd: file.as_raw_fd(),
    };
    // SAFETY: valid ruleset fd and a packed path-beneath attribute.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0u32,
        )
    };
    if rc != 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display())));
    }
    Ok(())
}

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// BPF program: kill on a foreign syscall ABI, `EPERM` for the denied calls,
/// allow everything else (the service unit's own filter still applies).
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn seccomp_filter() -> Result<Vec<libc::sock_filter>, String> {
    const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
    const RET_K: u16 = 0x06; // BPF_RET | BPF_K
    const OFFSET_NR: u32 = 0;
    const OFFSET_ARCH: u32 = 4;
    let stmt = |code, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
    let denied: [libc::c_long; 9] = [
        libc::SYS_socket,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_setsid,
        libc::SYS_setpgid,
    ];
    let mut prog = vec![
        stmt(LD_W_ABS, OFFSET_ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, libc::SECCOMP_RET_KILL_PROCESS),
        stmt(LD_W_ABS, OFFSET_NR),
    ];
    if cfg!(target_arch = "x86_64") {
        // x32 system calls share the x86_64 audit arch; refuse them.
        prog.push(jump(JGE_K, 0x4000_0000, 0, 1));
        prog.push(stmt(RET_K, libc::SECCOMP_RET_KILL_PROCESS));
    }
    for nr in denied {
        prog.push(jump(JEQ_K, nr as u32, 0, 1));
        prog.push(stmt(
            RET_K,
            libc::SECCOMP_RET_ERRNO | (libc::EPERM as u32 & libc::SECCOMP_RET_DATA),
        ));
    }
    prog.push(stmt(RET_K, libc::SECCOMP_RET_ALLOW));
    Ok(prog)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn seccomp_filter() -> Result<Vec<libc::sock_filter>, String> {
    Err("package checks are only supported on x86_64 and aarch64".into())
}

/// Run a confined command with a wall-clock timeout. stdout and stderr go to
/// files in `scratch` (at most `limit` bytes of each are read back) and the
/// whole process group is killed when the leader exits or times out.
pub fn run(
    cmd: &mut Command,
    confinement: &Confinement,
    scratch: &Path,
    timeout: Duration,
    limit: usize,
) -> Result<String, String> {
    let id = uuid::Uuid::new_v4();
    let out_path = scratch.join(format!(".rast-check-{id}.out"));
    let err_path = scratch.join(format!(".rast-check-{id}.err"));
    let create = |p: &Path| {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(p)
            .map_err(|e| format!("cannot create check output file: {e}"))
    };
    let stdout = create(&out_path)?;
    let stderr = create(&err_path)?;
    confinement.apply(cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let result = run_group(cmd, timeout);
    let read = |p: &Path| {
        let mut buf = Vec::new();
        if let Ok(f) = fs::File::open(p) {
            let _ = f.take(limit as u64).read_to_end(&mut buf);
        }
        let _ = fs::remove_file(p);
        buf
    };
    let mut output = read(&out_path);
    output.extend_from_slice(b"\n");
    output.extend_from_slice(&read(&err_path));
    let output = String::from_utf8_lossy(&output).into_owned();
    match result? {
        status if status.success() => Ok(output),
        status => Err(format!(
            "exited with {status}: {}",
            output
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>()
        )),
    }
}

fn run_group(cmd: &mut Command, timeout: Duration) -> Result<std::process::ExitStatus, String> {
    let mut child = cmd.spawn().map_err(|e| format!("cannot start: {e}"))?;
    let pid = child.id() as libc::pid_t;
    let started = Instant::now();
    let timed_out = loop {
        // Wait without reaping: while the leader is a zombie its process
        // group id cannot be reused, so the group kill below is exact.
        // SAFETY: zeroed siginfo is a valid out-parameter for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            kill_group(pid);
            let _ = child.wait();
            return Err(format!("cannot wait: {e}"));
        }
        // SAFETY: waitid filled `info` (si_pid is 0 when nothing changed).
        if unsafe { info.si_pid() } != 0 {
            break false;
        }
        if started.elapsed() > timeout {
            break true;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Kill whatever the check left behind (or the whole check on timeout).
    kill_group(pid);
    let status = child.wait().map_err(|e| format!("cannot wait: {e}"))?;
    if timed_out {
        return Err(format!("timed out after {}s", timeout.as_secs()));
    }
    Ok(status)
}

fn kill_group(pgid: libc::pid_t) {
    // SAFETY: signalling our own child's process group.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let home = tmp.path().join("home");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&home).unwrap();
        (tmp, root, home)
    }

    fn exec(prog: &Path, root: &Path, home: &Path, timeout: Duration) -> Result<String, String> {
        let confinement = Confinement::new(root, home, false).unwrap();
        let mut cmd = Command::new(prog);
        cmd.current_dir(root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin");
        run(&mut cmd, &confinement, home, timeout, 16 * 1024)
    }

    #[test]
    fn runs_a_check_and_reads_both_streams() {
        let (_tmp, root, home) = dirs();
        let prog = script(&root, "demo", "echo demo 1.0.0; echo warning >&2");
        let out = exec(&prog, &root, &home, Duration::from_secs(10)).unwrap();
        assert!(
            out.contains("demo 1.0.0") && out.contains("warning"),
            "{out}"
        );
        // Output files are cleaned up.
        assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    }

    #[test]
    fn a_background_child_cannot_hold_the_job() {
        let (_tmp, root, home) = dirs();
        let marker = home.join("survived");
        let prog = script(
            &root,
            "demo",
            &format!("(sleep 3; touch {}) &\necho demo 1.0.0", marker.display()),
        );
        let started = Instant::now();
        let out = exec(&prog, &root, &home, Duration::from_secs(10)).unwrap();
        assert!(out.contains("demo 1.0.0"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "the background child was not killed");
    }

    #[test]
    fn a_hanging_check_times_out_with_its_children() {
        let (_tmp, root, home) = dirs();
        let marker = home.join("survived");
        let prog = script(
            &root,
            "demo",
            &format!("(sleep 2; touch {}) &\nsleep 30", marker.display()),
        );
        let started = Instant::now();
        let err = exec(&prog, &root, &home, Duration::from_millis(500)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_secs(3));
        assert!(!marker.exists());
    }

    #[test]
    fn checks_cannot_open_sockets_or_leave_the_group() {
        let (_tmp, root, home) = dirs();
        // Python is the most portable way to make raw syscalls in a test.
        let Ok(python) = which("python3") else { return };
        let probe = root.join("probe.py");
        fs::write(
            &probe,
            // setsid is tried in a forked child: a group leader would get
            // EPERM even without the filter.
            "import os, socket, sys\n\
             def attempt(name, fn):\n\
             \x20   try:\n\
             \x20       fn()\n\
             \x20       print(name, \"allowed\")\n\
             \x20   except OSError as e:\n\
             \x20       print(name, \"denied\", e.errno)\n\
             \x20   sys.stdout.flush()\n\
             attempt(\"socket\", lambda: socket.socket(socket.AF_UNIX))\n\
             pid = os.fork()\n\
             if pid == 0:\n\
             \x20   attempt(\"setsid\", os.setsid)\n\
             \x20   attempt(\"setpgid\", lambda: os.setpgid(0, 0))\n\
             \x20   os._exit(0)\n\
             os.waitpid(pid, 0)\n",
        )
        .unwrap();
        let prog = script(
            &root,
            "demo",
            &format!("exec {python} -I -S {}", probe.display()),
        );
        let out = exec(&prog, &root, &home, Duration::from_secs(10)).unwrap();
        assert!(out.contains("socket denied 1"), "{out}");
        assert!(out.contains("setsid denied 1"), "{out}");
        assert!(out.contains("setpgid denied 1"), "{out}");
    }

    #[test]
    fn landlock_limits_reads_and_writes() {
        if landlock_abi().is_none() {
            eprintln!("Landlock unavailable: skipping");
            return;
        }
        let (tmp, root, home) = dirs();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, "do not read").unwrap();
        let outside = tmp.path().join("outside.txt");
        let prog = script(
            &root,
            "demo",
            &format!(
                "cat {s} 2>/dev/null && echo read-secret\n\
                 echo x > {o} 2>/dev/null && echo wrote-outside\n\
                 echo x > {h}/ok && echo wrote-home\n\
                 echo x > {r}/planted 2>/dev/null && echo wrote-root\n\
                 cat /etc/hostname >/dev/null 2>&1 && echo read-etc\n\
                 echo done",
                s = secret.display(),
                o = outside.display(),
                h = home.display(),
                r = root.display(),
            ),
        );
        let out = exec(&prog, &root, &home, Duration::from_secs(10)).unwrap();
        assert!(out.contains("done"), "{out}");
        assert!(out.contains("wrote-home"), "{out}");
        for forbidden in ["read-secret", "wrote-outside", "wrote-root", "read-etc"] {
            assert!(!out.contains(forbidden), "{forbidden}: {out}");
        }
        assert!(!outside.exists());
    }

    fn which(name: &str) -> Result<String, ()> {
        ["/usr/bin", "/bin", "/usr/local/bin"]
            .iter()
            .map(|d| Path::new(d).join(name))
            .find(|p| p.is_file())
            .map(|p| p.display().to_string())
            .ok_or(())
    }
}
