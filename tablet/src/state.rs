//! Choices remembered across host reconnects and instance restarts: the
//! selected view and the pen, touch and display modes.

use crate::input::{PenMode, TouchMode};
use remdesktop_proto::DisplayMode;

const DIR: &str = "/home/root/.config/remdesktop";
const PATH: &str = "/home/root/.config/remdesktop/state";

#[derive(Debug, Default, Clone, PartialEq)]
pub struct State {
    /// Label of the selected view ("VIRT", "1", …); "" for the default.
    pub view: String,
    pub pen: Option<PenMode>,
    pub touch: Option<TouchMode>,
    pub display: DisplayMode,
    /// Pen strokes are previewed on the panel at once.
    pub ink: bool,
}

impl State {
    pub fn load() -> State {
        let mut s = State::default();
        for line in std::fs::read_to_string(PATH).unwrap_or_default().lines() {
            match line.split_once('=') {
                Some(("view", v)) => s.view = v.into(),
                Some(("pen", "tablet")) => s.pen = Some(PenMode::Tablet),
                Some(("pen", "mouse")) => s.pen = Some(PenMode::Mouse),
                Some(("touch", "touchpad")) => s.touch = Some(TouchMode::Touchpad),
                Some(("touch", "touchscreen")) => s.touch = Some(TouchMode::Touchscreen),
                Some(("display", "none")) => s.display = DisplayMode::None,
                Some(("display", "edge")) => s.display = DisplayMode::Edge,
                Some(("ink", v)) => s.ink = v == "on",
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let pen = match self.pen {
            Some(PenMode::Mouse) => "mouse",
            _ => "tablet",
        };
        let touch = match self.touch {
            Some(TouchMode::Touchscreen) => "touchscreen",
            _ => "touchpad",
        };
        let display = match self.display {
            DisplayMode::Normal => "normal",
            DisplayMode::None => "none",
            DisplayMode::Edge => "edge",
        };
        let ink = if self.ink { "on" } else { "off" };
        let text = format!("view={}\npen={pen}\ntouch={touch}\ndisplay={display}\nink={ink}\n", self.view);
        if let Err(e) = std::fs::create_dir_all(DIR).and_then(|_| std::fs::write(PATH, text)) {
            log!("remdesktop: saving {PATH}: {e}");
        }
    }
}
