mod args;
mod desktop;
mod device;
mod files;
mod macros;
mod profiles;
mod session;
mod settings;

use std::io::{self, Write};

use args::{HELP, Options, help, input};

pub(crate) fn run(args: &[&str], out: &mut dyn Write) -> io::Result<()> {
    match args {
        ["desktop-session-version"] => return writeln!(out, "1"),
        ["desktop-session", token] => return session::broker(token, out),
        ["desktop-session-connect", token, arguments @ ..] => {
            return session::connect(token, arguments, out);
        }
        _ => {}
    }
    if let ["desktop-device", args @ ..] = args {
        return desktop::device_command(args, out);
    }
    let (options, args) = Options::parse(args)?;
    match args {
        [] | ["help" | "--help" | "-h"] => out.write_all(HELP.as_bytes()),
        ["help", topic] | [topic, "--help" | "-h"] => out.write_all(help(topic).as_bytes()),
        ["--version" | "-V"] => writeln!(
            out,
            "{} {}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        ),
        ["desktop-list"] => desktop::list(&options, out),
        ["desktop-models"] => desktop::models(out),
        ["desktop-settings"] => desktop::settings(&options, out),
        ["desktop-inputs"] => desktop::inputs(out),
        ["desktop-record", args @ ..] => desktop::record(args, out),
        ["desktop-recovery-list"] => desktop::recovery_list(out),
        ["desktop-recovery", args @ ..] => desktop::recover(args, out),
        ["desktop-watch", args @ ..] => device::watch_usb_endpoints(&options, args, out, true),
        ["desktop-status", args @ ..] => device::status(&options, args, out, true),
        ["desktop-observe-status", args @ ..] => device::observe(&options, args, out, true),
        ["observe-status", args @ ..] => device::observe(&options, args, out, false),
        ["list", "models"] => device::catalog(out),
        ["desktop-pointer-activity"] => desktop::pointer_activity(&options, out),
        ["list", "watch", args @ ..] => device::watch_usb_endpoints(&options, args, out, false),
        ["list"] => device::list(&options, out),
        ["recovery", args @ ..] => device::recovery_command(&options, args, out),
        ["info"] => device::info(&options, out),
        ["status", args @ ..] => device::status(&options, args, out, false),
        ["settings"] => settings::show(&options, out),
        ["polling", args @ ..] => settings::polling_command(&options, args, out),
        ["dpi", args @ ..] => settings::dpi_command(&options, args, out),
        ["sensor", args @ ..] => settings::sensor_command(&options, args, out),
        ["timing", args @ ..] => settings::timing_command(&options, args, out, false),
        ["lighting", args @ ..] => settings::timing_command(&options, args, out, true),
        ["button", args @ ..] => settings::button_command(&options, args, out),
        ["profile", args @ ..] => profiles::profile_command(&options, args, out),
        ["macro", args @ ..] => macros::macro_command(&options, args, out),
        ["reset"] => settings::reset(&options, out),
        ["reset", "--keep-macros"] => settings::reset_settings_only(&options, out),
        _ => Err(input("unknown command or arguments; run --help")),
    }
}
