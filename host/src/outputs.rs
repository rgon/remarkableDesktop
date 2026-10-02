//! Monitor layout in compositor logical coordinates, via xdg-output.

use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

#[derive(Debug, Clone, Default)]
pub struct Output {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Default)]
struct State {
    outputs: Vec<(wl_output::WlOutput, Output)>,
    manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_output" => {
                    let o = reg.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
                    s.outputs.push((o, Output::default()));
                }
                "zxdg_output_manager_v1" => s.manager = Some(reg.bind(name, version.min(3), qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(s: &mut Self, o: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = ev {
            if let Some((_, out)) = s.outputs.iter_mut().find(|(w, _)| w == o) {
                out.name = name;
            }
        }
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for State {
    fn event(_: &mut Self, _: &zxdg_output_manager_v1::ZxdgOutputManagerV1, _: zxdg_output_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for State {
    fn event(s: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, ev: zxdg_output_v1::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let out = &mut s.outputs[*idx].1;
        match ev {
            zxdg_output_v1::Event::LogicalPosition { x, y } => (out.x, out.y) = (x, y),
            zxdg_output_v1::Event::LogicalSize { width, height } => (out.w, out.h) = (width, height),
            zxdg_output_v1::Event::Name { name } if out.name.is_empty() => out.name = name,
            _ => {}
        }
    }
}

/// Current outputs, ordered left to right, then top to bottom.
pub fn list() -> Result<Vec<Output>, String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("wayland: {e}"))?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state).map_err(|e| format!("wayland: {e}"))?;
    let manager = state.manager.clone().ok_or("compositor lacks xdg-output")?;
    for i in 0..state.outputs.len() {
        manager.get_xdg_output(&state.outputs[i].0, &qh, i);
    }
    queue.roundtrip(&mut state).map_err(|e| format!("wayland: {e}"))?;
    let mut outs: Vec<Output> = state.outputs.into_iter().map(|(_, o)| o).filter(|o| o.w > 0 && o.h > 0).collect();
    outs.sort_by_key(|o| (o.x, o.y));
    Ok(outs)
}
