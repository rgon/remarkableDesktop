//! Keeps the user's monitor layout when a virtual monitor appears.
//!
//! Mutter has no stored layout for "your monitors + our virtual one", so it
//! falls back to a default row (dropping rotations). We snapshot the layout
//! beforehand and re-apply it, temporarily, with the virtual monitor added
//! at the right edge. When the virtual monitor goes away Mutter restores the
//! stored layout by itself.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

type MonitorSpec = (String, String, String, String); // connector, vendor, product, serial
type Mode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type Monitor = (MonitorSpec, Vec<Mode>, HashMap<String, OwnedValue>);
type Logical = (i32, i32, f64, u32, bool, Vec<MonitorSpec>, HashMap<String, OwnedValue>);
type State = (u32, Vec<Monitor>, Vec<Logical>, HashMap<String, OwnedValue>);

/// Monitor-config methods: 1 = temporary (not written to monitors.xml).
const TEMPORARY: u32 = 1;

fn proxy(conn: &Connection) -> zbus::Result<Proxy<'_>> {
    Proxy::new(conn, "org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig", "org.gnome.Mutter.DisplayConfig")
}

fn state(conn: &Connection) -> zbus::Result<State> {
    proxy(conn)?.call("GetCurrentState", &())
}

/// The layout to restore: logical monitors as returned by Mutter.
pub struct Layout {
    logical: Vec<Logical>,
}

pub fn snapshot(conn: &Connection) -> Result<Layout, String> {
    let (_, _, logical, _) = state(conn).map_err(|e| format!("GetCurrentState: {e}"))?;
    Ok(Layout { logical })
}

fn current_mode(monitors: &[Monitor], connector: &str) -> Option<(String, i32, i32)> {
    let (_, modes, _) = monitors.iter().find(|m| m.0.0 == connector)?;
    let current = |m: &&Mode| m.6.get("is-current").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
    modes.iter().find(current).or(modes.first()).map(|m| (m.0.clone(), m.1, m.2))
}

/// Waits for a monitor not in `layout` to appear (the virtual one), then
/// re-applies `layout` with it placed to the right of everything, top-aligned
/// with the rightmost monitor. Returns the new monitor's connector.
pub fn keep_layout(conn: &Connection, layout: &Layout, wait: Duration) -> Result<String, String> {
    let known: Vec<&str> = layout.logical.iter().flat_map(|l| l.5.iter().map(|s| s.0.as_str())).collect();
    let deadline = Instant::now() + wait;
    let (serial, monitors, props) = loop {
        let (serial, monitors, _, props) = state(conn).map_err(|e| format!("GetCurrentState: {e}"))?;
        if monitors.iter().any(|m| !known.contains(&m.0.0.as_str())) {
            break (serial, monitors, props);
        }
        if Instant::now() > deadline {
            return Err("virtual monitor did not appear".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let new = monitors.iter().find(|m| !known.contains(&m.0.0.as_str())).unwrap().0.0.clone();
    // In "physical" layout mode (2) positions are in device pixels, unscaled.
    let physical = props.get("layout-mode").and_then(|v| u32::try_from(v).ok()) == Some(2);

    let mut config: Vec<(i32, i32, f64, u32, bool, Vec<(String, String, HashMap<String, Value>)>)> = vec![];
    // Logical sizes for placement: mode size, swapped for 90/270 transforms, divided by scale.
    let mut right = (i32::MIN, 0); // (right edge, top of the monitor owning it)
    for (x, y, scale, transform, primary, specs, _) in &layout.logical {
        let mut mons = vec![];
        for spec in specs {
            let (mode, w, h) = current_mode(&monitors, &spec.0).ok_or_else(|| format!("no mode for {}", spec.0))?;
            let w = if transform % 2 == 1 { h } else { w };
            let edge = x + if physical { w } else { (w as f64 / scale).round() as i32 };
            if edge > right.0 {
                right = (edge, *y);
            }
            mons.push((spec.0.clone(), mode, HashMap::new()));
        }
        config.push((*x, *y, *scale, *transform, *primary, mons));
    }
    let (mode, _, _) = current_mode(&monitors, &new).ok_or("no mode for the virtual monitor")?;
    config.push((right.0, right.1, 1.0, 0, false, vec![(new.clone(), mode, HashMap::new())]));

    let props: HashMap<&str, Value> = HashMap::new();
    proxy(conn)
        .and_then(|p| p.call_method("ApplyMonitorsConfig", &(serial, TEMPORARY, config, props)))
        .map_err(|e| format!("ApplyMonitorsConfig: {e}"))?;
    Ok(new)
}

/// EDID identity of each connector: (vendor, product, serial), the format
/// GNOME's per-device `output` settings use.
pub fn edids() -> Result<HashMap<String, (String, String, String)>, String> {
    let conn = Connection::session().map_err(|e| format!("session bus: {e}"))?;
    let (_, monitors, _, _) = state(&conn).map_err(|e| format!("GetCurrentState: {e}"))?;
    Ok(monitors.into_iter().map(|(spec, _, _)| (spec.0, (spec.1, spec.2, spec.3))).collect())
}
