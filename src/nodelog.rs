//! A copy of everything the running node prints, in `~/.sermonindex/node.log`.
//!
//! WHY
//!
//! The menu shows what the node is doing in its activity panel, whether the
//! node was started from the menu (in the background, output going nowhere),
//! by systemd (output going to the journal, which a desktop user often cannot
//! read), or in another terminal. The one place all three can agree on is a
//! file the node writes itself.
//!
//! HOW
//!
//! The node prints with println!/eprintln! from hundreds of places, and the
//! torrent library logs on its own. Rather than touch each of them, `start`
//! points stdout and stderr at a pipe, and one thread copies the pipe to the
//! original destination (terminal, journal, /dev/null) AND to the log. Output
//! keeps going exactly where it went before; the file is extra.
//!
//! The log is kept small: at 4 MB it becomes node.log.1 and a fresh one
//! starts, so it never holds more than ~8 MB.

#[cfg(unix)]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::thread::JoinHandle;
    use std::time::Duration;

    const MAX: u64 = 4 * 1024 * 1024;

    struct Tee {
        orig_out: i32,
        orig_err: i32,
        thread: Option<JoinHandle<()>>,
    }

    static TEE: Mutex<Option<Tee>> = Mutex::new(None);

    pub fn path() -> PathBuf {
        crate::config::data_dir().join("node.log")
    }

    fn open_log() -> Option<File> {
        let _ = std::fs::create_dir_all(crate::config::data_dir());
        OpenOptions::new().create(true).append(true).open(path()).ok()
    }

    fn cloexec(fd: i32) {
        unsafe {
            let fl = libc::fcntl(fd, libc::F_GETFD);
            if fl >= 0 {
                libc::fcntl(fd, libc::F_SETFD, fl | libc::FD_CLOEXEC);
            }
        }
    }

    pub fn install() {
        let Some(mut log) = open_log() else { return };
        let mut written = log.metadata().map(|m| m.len()).unwrap_or(0);
        let mut fds = [0i32; 2];
        unsafe {
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return;
            }
            let orig_out = libc::dup(1);
            let orig_err = libc::dup(2);
            if orig_out < 0 || orig_err < 0 {
                libc::close(fds[0]);
                libc::close(fds[1]);
                return;
            }
            // Children (the upgrade, a browser) get the real outputs, not the pipe.
            cloexec(fds[0]);
            cloexec(orig_out);
            cloexec(orig_err);
            libc::dup2(fds[1], 1);
            libc::dup2(fds[1], 2);
            libc::close(fds[1]);

            let read_fd = fds[0];
            let out_fd = libc::dup(orig_out);
            cloexec(out_fd);
            let thread = std::thread::Builder::new()
                .name("nodelog".into())
                .spawn(move || {
                    let mut rd = File::from_raw_fd(read_fd);
                    let mut out = File::from_raw_fd(out_fd);
                    let mut buf = [0u8; 16 * 1024];
                    loop {
                        let n = match rd.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        };
                        // Never let a broken terminal or full disk stop the node:
                        // errors on either copy are ignored.
                        let _ = out.write_all(&buf[..n]);
                        let _ = log.write_all(&buf[..n]);
                        written += n as u64;
                        if written > MAX {
                            let p = path();
                            let _ = std::fs::rename(&p, p.with_extension("log.1"));
                            if let Some(l) = open_log() {
                                log = l;
                            }
                            written = 0;
                        }
                    }
                })
                .ok();
            *TEE.lock().unwrap() = Some(Tee { orig_out, orig_err, thread });
        }

        // A panic's message would otherwise sit in the pipe while the process
        // exits; write it straight to both places instead.
        std::panic::set_hook(Box::new(|info| {
            let msg = format!("\n[node] crashed: {info}\n");
            if let Ok(g) = TEE.lock() {
                if let Some(t) = g.as_ref() {
                    unsafe {
                        libc::write(t.orig_err, msg.as_ptr().cast(), msg.len());
                    }
                }
            }
            if let Some(mut l) = open_log() {
                let _ = l.write_all(msg.as_bytes());
            }
        }));
    }

    /// Put the real outputs back and let the copier drain what is left, so the
    /// last lines ("shutting down") reach the log too.
    pub fn finish() {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        let Some(mut t) = TEE.lock().ok().and_then(|mut g| g.take()) else { return };
        unsafe {
            libc::dup2(t.orig_out, 1);
            libc::dup2(t.orig_err, 2);
        }
        if let Some(h) = t.thread.take() {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !h.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[cfg(unix)]
pub use imp::{finish, install, path};

#[cfg(not(unix))]
pub fn install() {}
#[cfg(not(unix))]
pub fn finish() {}
#[cfg(not(unix))]
pub fn path() -> std::path::PathBuf {
    crate::config::data_dir().join("node.log")
}
