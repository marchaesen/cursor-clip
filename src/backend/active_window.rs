//! Detect the currently focused (activated) toplevel's `app_id` so instant paste
//! can choose the correct shortcut. GUI apps paste with Ctrl+V, but terminals use
//! Ctrl+Shift+V. On COSMIC we read the active window via `zcosmic_toplevel_info_v1`
//! (which extends `ext_foreign_toplevel_list_v1`), pick the toplevel whose state
//! includes `activated`, and look up its `app_id`.

use std::collections::HashMap;

use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};

use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1},
    zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
};
use log::debug;
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, EVT_TOPLEVEL_OPCODE, ExtForeignToplevelListV1},
};

// Matches the `activated` entry of zcosmic_toplevel_handle_v1's state enum.
const STATE_ACTIVATED: u32 = 2;

#[derive(Default)]
struct ActiveWindowState {
    info: Option<ZcosmicToplevelInfoV1>,
    /// ext foreign toplevel handle id -> app_id
    app_ids: HashMap<ObjectId, String>,
    /// cosmic toplevel handle id -> ext foreign toplevel handle id
    cosmic_to_ext: HashMap<ObjectId, ObjectId>,
    /// ext handle id of the toplevel currently reported as activated
    activated_ext: Option<ObjectId>,
    /// set once the info object emits `done`, i.e. the initial state burst
    /// (including every handle's `state` event) has been fully delivered
    done: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ActiveWindowState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for ActiveWindowState {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            // Ask for the cosmic extension object so we receive its `state` events,
            // and remember which ext handle it maps back to.
            if let Some(info) = &state.info {
                let cosmic = info.get_cosmic_toplevel(&toplevel, qh, ());
                state.cosmic_to_ext.insert(cosmic.id(), toplevel.id());
            }
        }
    }

    event_created_child!(ActiveWindowState, ExtForeignToplevelListV1, [
        EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for ActiveWindowState {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_handle_v1::Event::AppId { app_id } = event {
            state.app_ids.insert(proxy.id(), app_id);
        }
    }
}

impl Dispatch<ZcosmicToplevelInfoV1, ()> for ActiveWindowState {
    fn event(
        state: &mut Self,
        _proxy: &ZcosmicToplevelInfoV1,
        event: zcosmic_toplevel_info_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // `done` is emitted once the current atomic batch of toplevel/state
        // changes has been fully sent. We use the first one as the signal that
        // every handle's initial `state` event has arrived.
        if let zcosmic_toplevel_info_v1::Event::Done = event {
            state.done = true;
        }
    }

    // Deprecated since v2 (we bind v2+, so this never fires), but the interface
    // declares a child-creating event so it must be registered.
    event_created_child!(ActiveWindowState, ZcosmicToplevelInfoV1, [
        zcosmic_toplevel_info_v1::EVT_TOPLEVEL_OPCODE => (ZcosmicToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZcosmicToplevelHandleV1, ()> for ActiveWindowState {
    fn event(
        state: &mut Self,
        proxy: &ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zcosmic_toplevel_handle_v1::Event::State { state: states } = event {
            // `states` is a packed array of u32 state values in native byte order.
            let activated = states
                .chunks_exact(4)
                .any(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == STATE_ACTIVATED);
            if activated && let Some(ext_id) = state.cosmic_to_ext.get(&proxy.id()) {
                state.activated_ext = Some(ext_id.clone());
            }
        }
    }
}

/// Returns the `app_id` of the currently focused toplevel, or `None` if it can't
/// be determined (protocol unavailable, no activated toplevel, etc.).
pub fn focused_app_id() -> Option<String> {
    let conn = Connection::connect_to_env().ok()?;
    let (globals, mut queue) = registry_queue_init::<ActiveWindowState>(&conn).ok()?;
    let qh = queue.handle();

    let mut state = ActiveWindowState::default();

    // Bind v2+ so we use `get_cosmic_toplevel` / `ext_foreign_toplevel_list` flow.
    let info = globals
        .bind::<ZcosmicToplevelInfoV1, _, _>(&qh, 2..=3, ())
        .ok()?;
    state.info = Some(info);
    let _list = globals
        .bind::<ExtForeignToplevelListV1, _, _>(&qh, 1..=1, ())
        .ok()?;

    // The first roundtrip delivers toplevels + app_ids and queues
    // get_cosmic_toplevel; the cosmic `state` events and the info `done` follow
    // on a later cosmic-comp event-loop tick. Loop (spacing roundtrips with short
    // sleeps) until `done` confirms the full state burst arrived, with a bounded
    // budget so we never hang if it never comes.
    for _ in 0..20 {
        if queue.roundtrip(&mut state).is_err() {
            break;
        }
        if state.done {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }

    let ext_id = state.activated_ext.as_ref()?;
    state.app_ids.get(ext_id).cloned()
}

/// True if `app_id` looks like a terminal emulator (which paste with Ctrl+Shift+V).
fn is_terminal(app_id: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "cosmicterm",
        "konsole",
        "alacritty",
        "kitty",
        "foot",
        "wezterm",
        "xterm",
        "terminator",
        "tilix",
        "ptyxis",
        "blackbox",
        "contour",
        "sakura",
        "guake",
        "yakuake",
        "terminal", // org.gnome.Terminal, xfce4-terminal, ...
        "console",  // org.gnome.Console (kgx)
    ];
    let a = app_id.to_lowercase();
    NEEDLES.iter().any(|n| a.contains(n))
}

/// True if the currently focused window is a terminal, so instant paste should
/// send Ctrl+Shift+V instead of Ctrl+V. Defaults to false when undetectable.
pub fn focused_window_is_terminal() -> bool {
    match focused_app_id() {
        Some(app_id) => {
            let terminal = is_terminal(&app_id);
            debug!("Focused window app_id={app_id:?} (terminal={terminal})");
            terminal
        }
        None => {
            debug!("Could not determine focused window app_id; assuming non-terminal");
            false
        }
    }
}
