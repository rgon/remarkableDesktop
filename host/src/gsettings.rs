//! Tells GNOME which monitor our absolute devices map to, through the same
//! per-device settings its Wacom/touchscreen panels use. Mutter otherwise
//! guesses: a tablet spans the whole desktop, a touchscreen goes to one
//! monitor it picks. Original values are put back on exit.

use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Tablet,
    Touchscreen,
}

fn location(kind: Kind, vendor: u16, product: u16) -> String {
    let (schema, dir) = match kind {
        Kind::Tablet => ("org.gnome.desktop.peripherals.tablet", "tablets"),
        Kind::Touchscreen => ("org.gnome.desktop.peripherals.touchscreen", "touchscreens"),
    };
    format!("{schema}:/org/gnome/desktop/peripherals/{dir}/{vendor:04x}:{product:04x}/")
}

fn get(loc: &str) -> Option<String> {
    let out = Command::new("gsettings").args(["get", loc, "output"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn set(loc: &str, value: &str) -> bool {
    Command::new("gsettings").args(["set", loc, "output", value]).status().is_ok_and(|s| s.success())
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Remembers what it changed so `Drop` can undo it.
#[derive(Default)]
pub struct Outputs {
    original: HashMap<String, String>,
}

impl Outputs {
    /// Maps a device to the monitor with this EDID, or back to automatic.
    pub fn map(&mut self, kind: Kind, vendor: u16, product: u16, edid: Option<&(String, String, String)>) -> bool {
        let loc = location(kind, vendor, product);
        if !self.original.contains_key(&loc) {
            let Some(old) = get(&loc) else { return false };
            self.original.insert(loc.clone(), old);
        }
        let value = match edid {
            Some((v, p, s)) => format!("[{}, {}, {}]", quote(v), quote(p), quote(s)),
            None => "['', '', '']".into(),
        };
        set(&loc, &value)
    }
}

impl Drop for Outputs {
    fn drop(&mut self) {
        for (loc, old) in &self.original {
            set(loc, old);
        }
    }
}
