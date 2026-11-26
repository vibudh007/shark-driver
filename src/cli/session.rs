use std::{
    env,
    io::{self, BufRead, BufReader, Read, Write},
    net::Shutdown,
    os::{
        fd::{AsRawFd, OwnedFd},
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    process::{Command, Stdio},
    time::Duration,
};

const READY: &str = "r1-session 1";
const EXIT: &str = "\u{1e}r1-exit ";
const MAX_ARGUMENTS: usize = 256;
const MAX_ARGUMENT_BYTES: usize = 8192;

fn address(token: &str) -> io::Result<SocketAddr> {
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(super::args::input("invalid session token"));
    }
    // Keep the version 1 address so the already-installed helper can serve new clients.
    SocketAddr::from_abstract_name(format!("attack-shark-r1-{token}").as_bytes())
}

fn read_arguments(stream: &mut UnixStream) -> io::Result<Vec<String>> {
    fn read_size(stream: &mut UnixStream) -> io::Result<usize> {
        let mut size = [0; 2];
        stream.read_exact(&mut size)?;
        Ok(usize::from(u16::from_be_bytes(size)))
    }
    let count = read_size(stream)?;
    if count == 0 || count > MAX_ARGUMENTS {
        return Err(super::args::input("invalid argument count"));
    }
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        let length = read_size(stream)?;
        if length > MAX_ARGUMENT_BYTES {
            return Err(super::args::input("session argument is too long"));
        }
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes)?;
        result
            .push(String::from_utf8(bytes).map_err(|error| super::args::input(error.to_string()))?);
    }
    Ok(result)
}

fn allowed(arguments: &[String]) -> bool {
    matches!(
        arguments.first().map(String::as_str),
        Some("desktop-device" | "desktop-record" | "desktop-recovery")
    )
}

fn serve(mut stream: UnixStream, uid: u32) -> io::Result<()> {
    if !same_user(&stream, uid)? {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let arguments = read_arguments(&mut stream)?;
    stream.set_read_timeout(None)?;
    if !allowed(&arguments) {
        writeln!(stream, "session command is outside the device scope")?;
        writeln!(stream, "{EXIT}2")?;
        return Ok(());
    }
    let channel = || stream.try_clone().map(|fd| Stdio::from(OwnedFd::from(fd)));
    let mut child = Command::new("/proc/self/exe")
        .args(arguments)
        .env_clear()
        .env("R1_TIMINGS", "1")
        .stdin(channel()?)
        .stdout(channel()?)
        .stderr(channel()?)
        .spawn()?;
    let mut lifetime = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        // Closing the lifetime pipe also closes this request's input, so an
        // interactive operation cannot keep waiting for confirmation.
        if unsafe { libc::poll(&mut lifetime, 1, 10) } > 0 {
            let _ = stream.shutdown(Shutdown::Both);
            if child.try_wait()?.is_none() {
                let _ = child.kill();
            }
            child.wait()?;
            return Ok(());
        }
    };
    writeln!(stream, "{EXIT}{}", status.code().unwrap_or(1))
}

fn same_user(stream: &UnixStream, uid: u32) -> io::Result<bool> {
    let mut credential = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // The kernel fills the live credential structure for this connected socket.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credential as *mut libc::ucred).cast(),
            &mut size,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(credential.uid == uid)
}

pub(super) fn broker(token: &str, out: &mut dyn Write) -> io::Result<()> {
    let uid = env::var("PKEXEC_UID")
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| super::args::input("start the session through desktop authorization"))?;
    let listener = UnixListener::bind_addr(&address(token)?)?;
    let mut poll = [
        libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    writeln!(out, "{READY}")?;
    out.flush()?;
    loop {
        // Both descriptors remain open for the loop; only readiness is inspected.
        if unsafe { libc::poll(poll.as_mut_ptr(), 2, -1) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if poll[0].revents != 0 {
            return Ok(());
        }
        if poll[1].revents != 0 {
            let (stream, _) = listener.accept()?;
            if let Err(error) = serve(stream, uid) {
                eprintln!("Shark Driver session: {error}");
            }
        }
    }
}

pub(super) fn connect(token: &str, arguments: &[&str], out: &mut dyn Write) -> io::Result<()> {
    if arguments.is_empty()
        || arguments.len() > MAX_ARGUMENTS
        || arguments.iter().any(|arg| arg.len() > MAX_ARGUMENT_BYTES)
    {
        return Err(super::args::input("invalid session request size"));
    }
    let mut stream = UnixStream::connect_addr(&address(token)?)?;
    stream.write_all(&(arguments.len() as u16).to_be_bytes())?;
    for arg in arguments {
        stream.write_all(&(arg.len() as u16).to_be_bytes())?;
        stream.write_all(arg.as_bytes())?;
    }
    let mut input = stream.try_clone()?;
    std::thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut input);
        let _ = input.shutdown(Shutdown::Write);
    });
    for line in BufReader::new(stream).lines() {
        let line = line?;
        if let Some(status) = line.strip_prefix(EXIT) {
            return if status == "0" {
                Ok(())
            } else {
                Err(io::Error::other("scoped operation failed"))
            };
        }
        if line.starts_with("r1-timing\t")
            || line.starts_with(concat!(env!("CARGO_PKG_NAME"), ":"))
            || line.starts_with("attack-shark-r1:")
        {
            eprintln!("{line}");
        } else {
            writeln!(out, "{line}")?;
            out.flush()?;
        }
    }
    Err(io::Error::other(
        "administrator session closed before verification",
    ))
}
