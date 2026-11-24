use std::{
    env, fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};

const PAYLOAD: &[(&str, &[u8], u32)] = &[
    ("app.py", include_bytes!("../../app.py"), 0o600),
    ("backend.py", include_bytes!("../../backend.py"), 0o600),
    (
        "macro_editor.py",
        include_bytes!("../../macro_editor.py"),
        0o600,
    ),
    ("streams.py", include_bytes!("../../streams.py"), 0o600),
    ("workflows.py", include_bytes!("../../workflows.py"), 0o600),
    (
        "icons/hicolor/scalable/apps/local.sharkdriver.SharkDriver.svg",
        include_bytes!("../../../design/logo/shark-silhouette.svg"),
        0o600,
    ),
    (
        "shark-driver-cli",
        include_bytes!("../../../target/release/shark-driver-cli"),
        0o700,
    ),
];

struct PayloadDirectory(PathBuf);

impl Drop for PayloadDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stage_payload(root: &Path) -> io::Result<PayloadDirectory> {
    let runtime = root.join(".shark-driver-runtime");
    match fs::DirBuilder::new().mode(0o700).create(&runtime) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if !fs::symlink_metadata(&runtime)?.is_dir() {
        return Err(io::Error::other(
            ".shark-driver-runtime must be a real directory",
        ));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let path = runtime.join(format!("{}-{}", std::process::id(), nonce.as_nanos()));
    fs::DirBuilder::new().mode(0o700).create(&path)?;
    let directory = PayloadDirectory(path);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory.0.join("icons/hicolor/scalable/apps"))?;
    for (name, content, mode) in PAYLOAD {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(*mode)
            .open(directory.0.join(name))?;
        file.write_all(content)?;
    }
    Ok(directory)
}

fn launch() -> io::Result<ExitCode> {
    let binary = env::current_exe()?.canonicalize()?;
    let root = binary
        .parent()
        .ok_or_else(|| io::Error::other("no application directory"))?;
    let root = match env::var_os("SHARK_DRIVER_DATA_ROOT") {
        Some(directory) => PathBuf::from(directory).canonicalize()?,
        None => root.to_path_buf(),
    };
    let payload = stage_payload(&root)?;
    let cli = payload.0.join("shark-driver-cli");
    let mut args = env::args_os().skip(1).peekable();
    let mut command = if args.peek().is_some_and(|arg| arg == "--cli") {
        args.next();
        Command::new(&cli)
    } else {
        let mut command = Command::new("/usr/bin/python3");
        command
            .args(["-B", "-E", "-s"])
            .arg(payload.0.join("app.py"));
        command
            .env("SHARK_DRIVER_ROOT", &root)
            .env("SHARK_DRIVER_CLI", &cli);
        command
    };
    let result = command.args(args).current_dir(&root).status()?;
    Ok(result
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from))
}

fn main() -> ExitCode {
    match launch() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("Shark Driver: {error}");
            ExitCode::FAILURE
        }
    }
}
