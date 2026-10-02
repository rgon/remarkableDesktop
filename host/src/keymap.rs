//! Which physical keys type `z` and `y` in the user's current keyboard
//! layout. Keyboards (USB HID and evdev alike) send key positions, not
//! letters, so on Dvorak or QWERTZ a fixed "Z key" produces something else.

use std::process::Command;
use xkbcommon::xkb;

/// The active GNOME input source as (layout, variant), e.g. ("us", "dvp").
fn active_layout() -> Option<(String, String)> {
    let out = Command::new("gsettings").args(["get", "org.gnome.desktop.input-sources", "mru-sources"]).output().ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !text.contains("'xkb'") {
        let out = Command::new("gsettings").args(["get", "org.gnome.desktop.input-sources", "sources"]).output().ok()?;
        text = String::from_utf8_lossy(&out.stdout).into_owned();
    }
    // [('xkb', 'us+dvp'), ('xkb', 'es')]: the first xkb entry is the active one.
    let start = text.find("('xkb', '")? + "('xkb', '".len();
    let id = &text[start..start + text[start..].find('\'')?];
    let (layout, variant) = id.split_once('+').unwrap_or((id, ""));
    Some((layout.to_string(), variant.to_string()))
}

/// Linux key codes producing `chars` (unshifted, first layout group), and
/// the layout they were computed for.
pub fn keys_for(chars: &[char]) -> Option<(Vec<u16>, String)> {
    let (layout, variant) = active_layout()?;
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(&ctx, "evdev", "", &layout, &variant, None, xkb::KEYMAP_COMPILE_NO_FLAGS)?;
    let (min, max) = (keymap.min_keycode().raw(), keymap.max_keycode().raw());
    let mut codes = vec![];
    for &c in chars {
        let want = xkb::utf32_to_keysym(c as u32);
        let kc = (min..=max).find(|&k| keymap.key_get_syms_by_level(xkb::Keycode::new(k), 0, 0).contains(&want))?;
        codes.push((kc - 8) as u16); // xkb keycodes are evdev codes + 8
    }
    let name = if variant.is_empty() { layout } else { format!("{layout}+{variant}") };
    Some((codes, name))
}

#[cfg(test)]
mod tests {
    /// Not a real assertion of the user's layout: just exercises xkbcommon.
    #[test]
    fn finds_keys_in_the_active_layout() {
        if let Some((codes, layout)) = super::keys_for(&['z', 'y']) {
            eprintln!("{layout}: z={} y={}", codes[0], codes[1]);
            assert_eq!(codes.len(), 2);
        }
    }
}
