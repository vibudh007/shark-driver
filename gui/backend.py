"""Run the Shark Driver CLI and decode its desktop-facing values."""
import os
from pathlib import Path
import re
import subprocess
import stat
import secrets
import time
from typing import NamedTuple

import workflows

ROOT = Path(os.environ.get("SHARK_DRIVER_ROOT", Path(__file__).resolve().parents[1])).resolve()
CLI = Path(os.environ.get("SHARK_DRIVER_CLI", ROOT / "target" / "release" / "shark-driver-cli"))
LIBRARY = ROOT / ".r1"
FLATPAK = Path("/.flatpak-info").is_file()
SYSTEM_HELPERS = (Path("/usr/bin/shark-driver-cli"), Path("/usr/bin/attack-shark-r1"))
ERROR_PREFIXES = ("shark-driver-cli: ", "attack-shark-r1: ")
SESSION = None
MODES = ("off", "static", "breathing", "neon", "color-breathing", "static-dpi", "breathing-dpi")


class CommandError(RuntimeError):
    """A CLI failure with its original diagnostics."""

    def __init__(self, message, status):
        for prefix in ERROR_PREFIXES:
            message = message.removeprefix(prefix)
        super().__init__(message.strip())
        self.status = status


def project_file(value):
    """Resolve file chooser paths within the project, including symlink checks."""
    path = Path(value).resolve()
    if not path.is_relative_to(ROOT):
        raise ValueError("Choose a file inside the project.")
    return str(path)


def writes_device(arguments):
    """Identify device writes that must be allowed to finish and verify."""
    match list(arguments):
        case ["reset"] | ["dpi", "select", _] | ["profile", "apply", _] | ["desktop-apply", *_]:
            return True
        case ["desktop-device", _, _, _, "desktop-apply", *_]:
            return True
        case ["desktop-profile-apply"] | ["desktop-macro", *_] | ["desktop-quick", *_]:
            return True
        case ["desktop-device", _, _, _, "desktop-quick", *_]:
            return True
        case ["macro", "upload" | "install" | "assign" | "restore-raw", *_]:
            return True
        case ["dpi" | "polling" | "sensor" | "lighting" | "timing" | "button", "set", *_]:
            return True
        case _:
            return False


def session_ready():
    return SESSION is not None and SESSION[1].poll() is None


def host_command(*arguments):
    """Use the host only for the fixed system helper and Polkit commands."""
    if FLATPAK:
        return ["/usr/bin/flatpak-spawn", "--host", "--directory=/", *arguments]
    return list(arguments)


def session_supported():
    helper = trusted_install()
    if helper is None:
        return False
    try:
        result = subprocess.run(host_command(str(helper), "desktop-session-version"),
                                capture_output=True, timeout=2, check=False)
        return result.stdout == b"1\n"
    except (OSError, subprocess.TimeoutExpired):
        return False


def start_session():
    """Authenticate once and hold the privileged worker's lifetime pipe open."""
    global SESSION
    if session_ready():
        return
    helper = trusted_install()
    if helper is None or not session_supported():
        raise CommandError("Install the updated administrator helper to enable session access.", 1)
    token = secrets.token_hex(32)
    process = subprocess.Popen(
        host_command("/usr/bin/pkexec", "--disable-internal-agent", str(helper), "desktop-session", token),
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    )
    if process.stdout.readline() != b"r1-session 1\n":
        process.stdin.close()
        raise CommandError("Administrator access was canceled or unavailable.", process.wait())
    process.stdout.close()
    SESSION = (token, process)


def stop_session():
    """Release the worker when the application shuts down."""
    global SESSION
    if SESSION:
        _, process = SESSION
        SESSION = None
        process.stdin.close()


def command_line(arguments, target=(), elevated=False):
    """Build an argument vector for the selected execution scope."""
    if elevated:
        helper = trusted_install()
        if helper is None:
            raise CommandError("Administrator access requires the packaged app.", 1)
        if SESSION is not None and not session_ready():
            raise CommandError("Administrator access ended. Reopen the app to authorize again.", 1)
        argv = ([project_file(CLI), "desktop-session-connect", SESSION[0]] if session_ready() else
                host_command("/usr/bin/pkexec", "--disable-internal-agent", str(helper)))
        if arguments[0] in ("desktop-record", "desktop-recovery") and not target:
            argv.extend(arguments)
        else:
            _, connection, _, node, _, identity = target
            argv.extend(["desktop-device", connection, node, identity, *arguments])
    else:
        argv = [project_file(CLI)]
        if arguments[0] != "desktop-device":
            argv.extend(["--data-dir", project_file(LIBRARY), *target])
        argv.extend(arguments)
    return argv


def run(arguments, target=(), *, timeout=40, elevated=False, input_text=None):
    """Execute one CLI request without a shell or inherited input."""
    started = time.monotonic_ns()
    result = subprocess.run(
        command_line(arguments, target, elevated),
        stdin=subprocess.DEVNULL if input_text is None else None,
        input=input_text.encode("utf-8") if input_text is not None else None,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        timeout=None if writes_device(arguments) else timeout,
        env={**os.environ, "LC_ALL": "C", "R1_TIMINGS": "1"}, check=False,
    )
    details = result.stderr.decode("utf-8", "replace").splitlines()
    if arguments[0] != "desktop-list":
        log_timings([line for line in details if line.startswith("r1-timing\t")],
                    f"gui.{arguments[0]}", started)
    if result.returncode:
        if elevated and result.returncode in (126, 127):
            raise CommandError("Administrator access was canceled or unavailable.", result.returncode)
        message = "\n".join(line for line in details if not line.startswith("r1-timing\t"))
        for prefix in ERROR_PREFIXES:
            message = message.removesuffix(f"\n{prefix}scoped operation failed")
        raise CommandError(message, result.returncode)
    return result.stdout.decode("utf-8")


def log_timings(lines, name, start):
    """Append timings without retaining device data or arguments."""
    try:
        folder = Path(project_file(ROOT / ".runtime"))
        folder.mkdir(exist_ok=True)
        with open(project_file(folder / "r1-timing.tsv"), "a", encoding="utf-8") as output:
            output.write("\n".join([*lines, f"r1-timing\t{name}\t{(time.monotonic_ns()-start)//1000}"]) + "\n")
    except (OSError, ValueError):
        pass


class DeviceEntry(NamedTuple):
    label: str
    target: tuple
    model: str = ""
    configurable: bool = False
    status: bool = False
    polling_read: bool = False
    r1_candidate: bool = False
    polling_write: bool = False
    dpi_read: bool = False
    dpi_write: bool = False
    button_read: bool = False
    button_write: bool = False


CONNECTIONS = {"wired": "USB cable", "bluetooth": "Bluetooth", "receiver": "2.4 GHz receiver"}


def devices(output):
    """Decode transport, model and available operations in automatic selection order."""
    version, *entries = output.splitlines()
    if version not in ("r1-desktop\t1", "r1-desktop\t2", "r1-desktop\t3", "r1-desktop\t4", "r1-desktop\t5"):
        raise ValueError("Unsupported device discovery version")
    legacy = version.endswith("\t1")
    priority = {name: index for index, name in enumerate(CONNECTIONS)}
    records = []
    for entry in entries:
        row = entry.split("\t")
        if legacy and len(row) == 3:
            row.extend(("R1", "true", "true", "false", "false"))
        if version.endswith("\t2") and len(row) == 7:
            row.append("false")
        if not version.endswith("\t4") and len(row) == 8:
            row.extend(("false", "false"))
        if not version.endswith("\t5") and len(row) == 10:
            legacy_buttons = row[3] in ("V3 Pro", "V6", "V3") and row[6] == "true"
            row.extend(("true", "true") if legacy_buttons else ("false", "false"))
        if (len(row) != 12 or row[0] not in CONNECTIONS
                or any(flag not in ("true", "false") for flag in row[4:])):
            raise ValueError("Unrecognized device record")
        records.append(row)
    records.sort(key=lambda row: (priority[row[0]], row[1]))
    counts = {name: sum(row[0] == name for row in records) for name in CONNECTIONS}
    devices = []
    for (kind, endpoint, identity, model, configurable, status, polling_read, polling_write,
         dpi_read, dpi_write, button_read, button_write) in records:
        model = "" if model == "-" else model
        name = CONNECTIONS[kind]
        if not legacy and model:
            name = f"{model} · {name}"
        if counts[kind] > 1:
            name = f"{name} · {endpoint.rsplit('/', 1)[-1]}"
        target = (("--bluetooth", endpoint) if kind == "bluetooth" else
                  ("--connection", kind, "--device", endpoint, "--expect-device", identity))
        candidate = configurable == "true"
        devices.append(DeviceEntry(name, target, model, candidate and model == "R1",
                                   status == "true", polling_read == "true",
                                   kind == "receiver" and candidate and not model,
                                   polling_write == "true", dpi_read == "true", dpi_write == "true",
                                   button_read == "true", button_write == "true"))
    return devices


def dpi_stages(output):
    """Decode the CLI stage list after a read or verified write."""
    stages = {}
    for line in output.splitlines():
        match = re.fullmatch(r"Stage ([1-8]): (unknown|[0-9]{1,5}) DPI, (on|off), #([0-9a-f]{6})( \(active\))?", line)
        if match is None:
            raise ValueError("Couldn’t read the mouse’s sensitivity levels.")
        index, dpi, enabled, color, active = match.groups()
        index = int(index)
        if index in stages:
            raise ValueError("The mouse returned a duplicate sensitivity level.")
        stages[index] = (dpi if dpi != "unknown" else "", enabled, color, bool(active))
    if not stages or sum(stage[3] for stage in stages.values()) > 1:
        raise ValueError("Couldn’t read the mouse’s active sensitivity level.")
    return stages


def button_line(line):
    """Decode one visible button assignment from a scoped command."""
    names = {1: "left", 2: "right", 3: "middle", 4: "dpi", 7: "forward", 8: "back"}
    named = re.fullmatch(r"(left|right|middle|forward|back|dpi) \(([1-9][0-9]?)\): ([^\r\n]+)", line)
    if named:
        return named[1], named[3]
    record = re.fullmatch(r"Button ([1-9][0-9]?): ([^\r\n]+)", line)
    if record is None or int(record[1]) not in names:
        raise ValueError("Couldn’t read the mouse’s button assignment.")
    return names[int(record[1])], record[2]


def vendor_models():
    """Return the display names and slot numbers from the CLI's model catalog."""
    lines = run(["desktop-models"]).splitlines()
    if lines[:1] != ["r1-models\t1"]:
        raise ValueError("Unsupported model catalog")
    models = []
    for line in lines[1:]:
        slot, name = line.split("\t", 1)
        if not slot.isdecimal() or not name:
            raise ValueError("Invalid model catalog")
        models.append((slot, name))
    return models


def windows_profile_names(contents, default):
    """Decode profile names and keep the active Windows index zero-based."""
    lines = contents.splitlines()
    if not lines or not lines[0].startswith("Windows active index: "):
        return 0, []
    indices = lines[0].removeprefix("Windows active index: ").split()
    active_index = int(indices[0]) if indices and indices[0].isdecimal() else 0
    entries = []
    for line in lines[1:]:
        index, separator, name = line.partition(": ")
        if separator and index.isdecimal():
            entries.append((index, workflows.windows_name(name) or f"{default} {index}"))
    return active_index, entries


def identify_authorized_receivers(entries):
    """Use read-only scoped status to identify R1 when unprivileged hidraw is unavailable."""
    if not session_ready():
        return entries
    resolved = []
    for entry in entries:
        if entry.r1_candidate:
            try:
                output = run(["desktop-status", "--count", "1", "--timeout-ms", "350"],
                             entry.target, elevated=True, timeout=2)
            except (CommandError, OSError, subprocess.TimeoutExpired):
                pass
            else:
                lines = output.splitlines()
                if lines[:1] == ["r1-status\t1"] and any(event.split("\t", 1)[0] in {
                    "battery", "dpi", "light", "receipt", "polling", "battery-notice", "profile", "other"
                } for event in lines[1:]):
                    entry = entry._replace(label=f"R1 · {entry.label}", model="R1",
                                           configurable=True, r1_candidate=False)
        resolved.append(entry)
    return resolved


def capture(target, elevated=False):
    """Retain both the decoded view and the exact verification baseline."""
    text = run(["desktop-settings"], target, elevated=elevated)
    return settings(text), text


def apply_settings(target, baseline, pending, elevated=False):
    """Use one scoped CLI invocation for an identified USB Apply."""
    if target[:1] == ("--connection",):
        batch = ["desktop-apply", baseline]
        for index, (command, values) in enumerate(pending.items()):
            if index:
                batch.append("--next")
            batch.extend((*command, *values))
        request = batch if elevated else ["desktop-device", target[1], target[3], target[5], *batch]
        text = run(request, target if elevated else (), elevated=elevated)
        return settings(text), text
    if run(["desktop-settings"], target) != baseline:
        raise ValueError("The mouse settings changed. Discard and refresh before applying.")
    for command, values in pending.items():
        run([*command, *values], target)
    return capture(target)


def quick_setting(target, baseline, command, elevated):
    """Refresh only the verified feature group affected by a single control."""
    if target[:1] != ("--connection",):
        return apply_settings(target, baseline, {tuple(command[:-1]): [command[-1]]}, elevated)
    args = ["desktop-quick", *command]
    if not elevated:
        args = ["desktop-device", target[1], target[3], target[5], *args]
    reply = run(args, target if elevated else (), elevated=elevated)
    version, *changes = reply.splitlines()
    if version != "r1-change\t1" or not changes:
        raise ValueError("No verified setting was returned")
    groups = {line.split("\t", 1)[0] for line in changes}
    rows = [line for line in baseline.splitlines() if line.split("\t", 1)[0] not in groups]
    # Preserve the canonical CLI ordering for future grouped baseline comparisons.
    order = {key: index for index, key in enumerate(
        ("r1-settings", "device", "sensor", "power", "lighting", "stage", "button"))}
    rows.extend(changes)
    rows.sort(key=lambda line: order[line.split("\t", 1)[0]])
    text = "\n".join(rows) + "\n"
    return settings(text), text


def scoped_action(args, target, elevated):
    """Keep file handling in the user session during privileged CLI actions."""
    if not elevated:
        return run(args, target, timeout=120)
    match args:
        case ["profile", "apply", name]:
            data = run(["profile", "export", name, "-"])
            return run(["desktop-profile-apply"], target, elevated=True, input_text=data)
        case ["profile", mode, name] if mode in ("save", "update", "defaults"):
            command = "desktop-profile-default" if mode == "defaults" else "desktop-profile-capture"
            data = run([command], target, elevated=True)
            return run(["profile", "store-capture", "save" if mode == "defaults" else mode, name],
                       input_text=data)
        case ["macro", "assign", button, name]:
            data = run(["macro", "export", name, "-"])
            return run(["desktop-macro", "install", button], target, elevated=True, input_text=data)
        case ["macro", verb, slot, path] if verb in ("upload", "install", "restore-raw"):
            with open(project_file(path), encoding="utf-8") as source:
                data = source.read(65_537)
            if len(data.encode("utf-8")) > 65_536:
                raise ValueError("The macro file is too large.")
            return run(["desktop-macro", verb, slot], target, elevated=True, input_text=data)
        case ["macro", "read", slot]:
            return run(["macro", "read", slot, "-"], target, elevated=True)
        case ["macro", "read-raw", slot, destination]:
            data = run(["macro", "read-raw", slot, "-"], target, elevated=True)
        case ["macro", "backup", destination, *slots]:
            data = run(["macro", "backup", "-", *slots], target, elevated=True, timeout=120)
        case _:
            return run(args, target, timeout=120, elevated=True)
    with open(project_file(destination), "x", encoding="utf-8") as output:
        output.write(data)
    return "Backup saved."


def trusted_install():
    """Find the preferred root-owned helper or the existing compatible installation."""
    for candidate in SYSTEM_HELPERS:
        if trusted_helper(candidate):
            return candidate
    return None


def trusted_helper(candidate):
    """Accept only a root-owned executable at the exact fixed system path."""
    if FLATPAK:
        candidate = Path("/run/host") / candidate.relative_to("/")
    try:
        path = candidate.resolve(strict=True)
        if path != candidate:
            return False
        info = path.stat()
        return (info.st_uid == 0 and stat.S_ISREG(info.st_mode)
                and info.st_mode & 0o022 == 0 and os.access(path, os.X_OK))
    except OSError:
        return False


def durable_backup(path, data):
    """Create a user-owned backup and sync it and its containing directory."""
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW
    with os.fdopen(os.open(path, flags, 0o600), "wb") as file:
        file.write(data)
        file.flush()
        os.fsync(file.fileno())
    directory = os.open(Path(path).parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def recover(node, port, backup, confirm, *, restore, elevated=False):
    """Keep the selected recovery session open until backup and confirmation finish."""
    if not re.fullmatch(r"/dev/hidraw[0-9]+", node):
        raise ValueError("Choose the detected recovery device.")
    backup = project_file(backup)
    command = ["desktop-recovery", "restore" if restore else "check", node, port]
    request = command_line(command, elevated=elevated)
    reports = []
    with subprocess.Popen(request, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, text=True, encoding="utf-8",
                          errors="replace") as process:
        for line in process.stdout:
            if line.startswith("r1-backup\t"):
                try:
                    encoded = line.removeprefix("r1-backup\t").strip()
                    if len(encoded) != 2 * 0x5e000:
                        raise ValueError("The recovery backup was incomplete.")
                    durable_backup(backup, bytes.fromhex(encoded))
                    process.stdin.write("SAVED\n")
                    process.stdin.flush()
                except Exception:
                    process.stdin.close()
                    raise
                reports.append(f"Saved a recovery backup to {backup}.\n")
                continue
            reports.append(line)
            if line.startswith("Type RECOVER to erase"):
                approved = confirm()
                if approved:
                    process.stdin.write("RECOVER\n")
                    process.stdin.flush()
                process.stdin.close()
                if not approved:
                    reports.append("Canceled. The backup is available.\n")
        status = process.wait()
    if status:
        if elevated and status in (126, 127):
            raise CommandError("Administrator access was canceled or unavailable.", status)
        raise CommandError("".join(reports[-5:]), status)
    return "".join(reports)


def settings(output):
    """Decode a complete settings snapshot and reject truncated results."""
    version, *rows = output.splitlines()
    if version != "r1-settings\t1":
        raise ValueError("Unsupported settings response version")
    fields, stages = {}, []
    for line in rows:
        match line.split("\t"):
            case ["device", firmware, hz]:
                fields.update(Firmware=firmware, Polling=f"{hz} Hz")
            case ["sensor", angle, ripple]:
                fields["Angle snap"] = "on" if angle == "true" else "off"
                fields["Ripple control"] = "on" if ripple == "true" else "off"
            case ["power", sleep, deep, debounce]:
                fields.update(Sleep=sleep, Debounce=debounce)
                fields["Deep sleep"] = deep
            case ["lighting", mode, brightness, speed, color]:
                number = int(mode)
                fields["Lighting"] = MODES[number] if number < len(MODES) else "unknown"
                fields.update(Brightness=brightness, Color=color)
                fields["Breath speed"] = speed
            case ["stage", index, dpi, enabled, color, active]:
                stages.append((int(index), int(dpi) if dpi else None, enabled == "true", color, active == "true"))
            case ["button", name, index, action]:
                fields[f"{name} ({index})"] = action
            case _:
                raise ValueError("Unrecognized settings field")
    required = ("Polling", "Angle snap", "Ripple control", "Sleep", "Deep sleep",
                "Debounce", "Lighting", "Brightness", "Breath speed", "Color")
    if [stage[0] for stage in stages] != list(range(1, 9)) or not all(k in fields for k in required):
        raise ValueError("Incomplete mouse settings")
    return fields, stages


def describe(error):
    """Translate common connection failures for the main view."""
    details = str(error)
    if details.startswith(("Administrator access", "The mouse settings changed", "Choose a file")):
        return details
    if "settings operations completed" in details:
        return "Some changes may have applied. Discard the draft and refresh to check the mouse."
    if "Permission denied" in details:
        return "Access to this device is required."
    if "FEE4" in details:
        return "Use USB or the receiver to change settings."
    if "no matching R1" in details or "disconnected" in details:
        return "Reconnect your mouse and refresh."
    if "erased-slot" in details:
        return "Macro writing is unavailable on this mouse."
    if "Windows macro record cannot preserve extended operands" in details:
        return "The Windows format can’t save this pointer movement. Use Export to keep it."
    if "File exists" in details:
        return "That name is already in use. Choose another name."
    if "unknown key" in details:
        return "Choose a key such as A, Enter, or Space."
    if isinstance(error, subprocess.TimeoutExpired):
        return "The request took too long. Refresh before trying again."
    return "The mouse or library couldn’t complete the request."


def device_open_denied(error):
    """Distinguish a denied open from a failure after a batch began."""
    detail = str(error)
    return (isinstance(error, CommandError) and "cannot open /dev/hidraw" in detail
            and "Permission denied" in detail and "settings operations completed" not in detail)
