//! Single instance: the first peek2 listens on a Unix socket; later
//! invocations hand it their command line and exit. One line per
//! connection: the arguments separated by NUL bytes.

use crate::rt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

pub fn socket_path() -> PathBuf {
    let display = std::env::var("DISPLAY").unwrap_or_default().replace('/', "_");
    crate::util::runtime_dir().join("peek2").join(format!("sock{display}"))
}

/// Pass `args` to a running instance. False if none is running.
pub fn forward(args: &[String]) -> bool {
    let Ok(mut s) = UnixStream::connect(socket_path()) else { return false };
    let line = args.join("\0");
    s.write_all(line.as_bytes()).and_then(|_| s.write_all(b"\n")).is_ok()
}

/// Become the instance. Errors if another one got there first.
pub fn listen() -> anyhow::Result<()> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(&path).is_ok() {
                anyhow::bail!("peek2 is already running");
            }
            // Left over from a crash.
            std::fs::remove_file(&path)?;
            UnixListener::bind(&path)?
        }
        Err(e) => return Err(e.into()),
    };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut line = String::new();
                let mut r = BufReader::new(&stream);
                if r.read_line(&mut line).is_ok() {
                    let args: Vec<String> = line.trim_end_matches('\n').split('\0').filter(|a| !a.is_empty()).map(String::from).collect();
                    drop(r);
                    rt::send(rt::Msg::Ipc(args, stream));
                }
            });
        }
    });
    Ok(())
}

pub fn cleanup() {
    let _ = std::fs::remove_file(socket_path());
}
