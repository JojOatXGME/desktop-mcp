//! `desktop-mcp fork`: daemonize the compositor. The parent stays attached to
//! the terminal only until the child reports readiness through a pipe, then
//! prints the environment exports (for `eval "$(desktop-mcp fork)"`) and
//! exits.

use std::fs::File;
use std::io::Read;
use std::os::fd::FromRawFd;

pub enum ForkOutcome {
    /// In the parent: read end of the readiness pipe.
    Parent(File),
    /// In the daemonized child: write end of the readiness pipe.
    Child(File),
}

pub fn fork_daemon() -> anyhow::Result<ForkOutcome> {
    let mut fds = [0i32; 2];
    // SAFETY: plain pipe/fork/setsid syscalls; fds array is valid.
    // O_CLOEXEC is essential: helper processes spawned by the daemon
    // (dbus-daemon, at-spi) must not inherit the write end, or the parent
    // would never see EOF.
    unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            anyhow::bail!("pipe failed: {}", std::io::Error::last_os_error());
        }
        match libc::fork() {
            -1 => anyhow::bail!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                libc::close(fds[0]);
                if libc::setsid() == -1 {
                    tracing::warn!("setsid failed: {}", std::io::Error::last_os_error());
                }
                Ok(ForkOutcome::Child(File::from_raw_fd(fds[1])))
            }
            _child_pid => {
                libc::close(fds[1]);
                Ok(ForkOutcome::Parent(File::from_raw_fd(fds[0])))
            }
        }
    }
}

/// Parent side: wait for the child's readiness report, relay it to stdout and
/// exit with a matching status.
pub fn parent_wait_and_print(mut pipe: File) -> ! {
    use std::io::Write as _;
    let mut report = String::new();
    let _ = pipe.read_to_string(&mut report);
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(report.as_bytes());
    let _ = stdout.flush();
    if report.contains("export ") {
        std::process::exit(0);
    }
    eprintln!("desktop-mcp: daemon failed to start (see log file)");
    std::process::exit(1);
}

/// Child side: detach stdio from the terminal, sending output to a log file.
pub fn redirect_stdio(log_path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    let log = File::options()
        .create(true)
        .append(true)
        .open(log_path)?;
    let null = File::options().read(true).open("/dev/null")?;
    // SAFETY: dup2 onto the standard fds with valid open files.
    unsafe {
        libc::dup2(null.as_raw_fd(), 0);
        libc::dup2(log.as_raw_fd(), 1);
        libc::dup2(log.as_raw_fd(), 2);
    }
    std::mem::forget(log);
    std::mem::forget(null);
    Ok(())
}
