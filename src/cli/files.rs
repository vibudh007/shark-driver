use std::{
    collections::HashSet,
    io::{self, Write},
    path::Path,
};

use shark_driver::profiles;

use super::args::{flag_pairs, input, number};

pub(super) fn write_file(out: &mut dyn Write, file: &str, bytes: &[u8]) -> io::Result<()> {
    if file == "-" {
        out.write_all(bytes)
    } else {
        profiles::write_new(Path::new(file), bytes)
    }
}

pub(super) fn print_names(out: &mut dyn Write, names: &[String]) -> io::Result<()> {
    for name in names {
        writeln!(out, "{name}")?;
    }
    Ok(())
}

pub(super) fn windows_library_names(names: &[&str]) -> io::Result<()> {
    let mut seen = HashSet::with_capacity(names.len());
    if names
        .iter()
        .any(|name| !seen.insert(name.to_ascii_lowercase()))
    {
        return Err(input(
            "Windows library names must be unique without regard to case",
        ));
    }
    Ok(())
}

pub(super) fn windows_model(
    args: &[&str],
) -> io::Result<Option<&'static shark_driver::models::Model>> {
    let flags = flag_pairs(args, &["--model"], true)?;
    flags
        .get("--model")
        .map(|slot| {
            shark_driver::models::Model::vendor_slot(number(slot, "Windows model slot")?)
                .ok_or_else(|| input("Windows model slot must be 1–18; use list models"))
        })
        .transpose()
}
