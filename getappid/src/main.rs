//! Standalone CLI: track the window (toplevel) currently **under the mouse
//! pointer** on the current COSMIC/Wayland session and reprint its info in
//! place whenever the pointer moves.
//!
//! Wayland deliberately does not expose the global pointer position to ordinary
//! clients, so the only reliable way to learn where the cursor is — and thus
//! which window sits under it — is to put a fullscreen, transparent overlay
//! surface on top of everything and read its pointer-motion events. We do that
//! with `zwlr_layer_shell_v1` (overlay layer, anchored to every edge), backed by
//! a full-screen wl_shm buffer we both keep transparent and paint the highlight
//! into.
//!
//! We separately read `zcosmic_toplevel_info_v1` to learn every window's
//! geometry (position + size), then hit-test the pointer against those
//! rectangles to decide which window is underneath. The same overlay is used to
//! paint a colored border around that window, so you can see what was detected.
//!
//! Only *toplevels* are tracked: a separate dialog window shows up as its own
//! entry, but popups/menus/tooltips/subsurfaces (xdg_popup, wl_subsurface) are
//! not exposed to external clients, so hovering one reports its parent toplevel.
//!
//!     cargo run            # from inside the getappid/ folder
//!     ./target/release/getappid
//!
//! NOTE: while running, the transparent overlay sits on top of every window and
//! captures pointer input, so clicks won't reach the windows underneath — this
//! is a hover-to-inspect tool. The keyboard is left alone, so Ctrl-C in the
//! launching terminal quits it. Geometry hit-testing assumes a single output
//! whose origin is (0, 0); on multi-monitor setups the coordinates may not line
//! up.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write as _;
use std::os::fd::{AsRawFd, BorrowedFd};

use memmap2::{MmapMut, MmapOptions};

use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_pointer::{self, WlPointer},
    wl_registry,
    wl_seat::{self, WlSeat},
    wl_output::WlOutput,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
};

use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1},
    zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, EVT_TOPLEVEL_OPCODE, ExtForeignToplevelListV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

// Values of zcosmic_toplevel_handle_v1's `state` enum.
const STATE_MAXIMIZED: u32 = 0;
const STATE_MINIMIZED: u32 = 1;
const STATE_ACTIVATED: u32 = 2;
const STATE_FULLSCREEN: u32 = 3;
const STATE_STICKY: u32 = 4;

/// Border thickness (px) and color (premultiplied ARGB8888, native u32) of the
/// rectangle we paint around the window under the cursor. 0xFF00E5FF == opaque
/// cyan.
const BORDER_THICKNESS: i32 = 4;
const BORDER_COLOR: u32 = 0xFF00_E5FF;

/// Everything we managed to collect about one toplevel window.
#[derive(Default)]
struct Window {
    app_id: Option<String>,
    title: Option<String>,
    identifier: Option<String>,
    states: Vec<u32>,
    /// (x, y, width, height) of the most recent geometry event, if any.
    geometry: Option<(i32, i32, i32, i32)>,
}

impl Window {
    fn has_state(&self, want: u32) -> bool {
        self.states.contains(&want)
    }

    /// True if (px, py) falls within this window's geometry rectangle.
    fn contains(&self, px: f64, py: f64) -> bool {
        match self.geometry {
            Some((x, y, w, h)) => {
                px >= x as f64
                    && py >= y as f64
                    && px < (x + w) as f64
                    && py < (y + h) as f64
            }
            None => false,
        }
    }

    fn state_labels(&self) -> String {
        let mut labels = Vec::new();
        if self.has_state(STATE_ACTIVATED) {
            labels.push("activated");
        }
        if self.has_state(STATE_MAXIMIZED) {
            labels.push("maximized");
        }
        if self.has_state(STATE_MINIMIZED) {
            labels.push("minimized");
        }
        if self.has_state(STATE_FULLSCREEN) {
            labels.push("fullscreen");
        }
        if self.has_state(STATE_STICKY) {
            labels.push("sticky");
        }
        if labels.is_empty() {
            "(none)".to_string()
        } else {
            labels.join(", ")
        }
    }
}

#[derive(Default)]
struct AppState {
    info: Option<ZcosmicToplevelInfoV1>,
    /// ext foreign toplevel handle id -> collected window info.
    windows: HashMap<ObjectId, Window>,
    /// cosmic toplevel handle id -> ext foreign toplevel handle id.
    cosmic_to_ext: HashMap<ObjectId, ObjectId>,
    /// Set once the info object emits `done`, i.e. the initial burst (including
    /// every handle's `state` event) has been fully delivered.
    done: bool,

    // --- overlay / pointer tracking ---
    /// The transparent fullscreen surface backing our overlay.
    overlay_surface: Option<WlSurface>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    /// wl_shm + the full-screen ARGB buffer we paint the highlight into.
    shm: Option<WlShm>,
    pool: Option<WlShmPool>,
    shm_file: Option<File>,
    mmap: Option<MmapMut>,
    buffer: Option<WlBuffer>,
    /// Size of the overlay/buffer in pixels (from the layer-surface configure).
    screen_w: i32,
    screen_h: i32,
    /// The rectangle currently painted, so we only repaint when it changes.
    last_rect: Option<(i32, i32, i32, i32)>,
    /// Forces the next render to repaint and re-commit even if the rectangle is
    /// unchanged (e.g. right after (re)creating the buffer, to map the surface).
    repaint: bool,
    /// True once the overlay has been configured and mapped.
    mapped: bool,
    /// Bound wl_output proxies, kept alive so cosmic-comp can reference them in
    /// toplevel `geometry` events (without these we never receive geometry).
    outputs: Vec<WlOutput>,
    /// Current pointer position in surface-local (== screen) coordinates.
    cursor: Option<(f64, f64)>,
    /// Set whenever something changed that should trigger a reprint.
    dirty: bool,
}

impl AppState {
    fn window_for_cosmic(&mut self, cosmic_id: &ObjectId) -> Option<&mut Window> {
        let ext_id = self.cosmic_to_ext.get(cosmic_id)?.clone();
        self.windows.get_mut(&ext_id)
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
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

impl Dispatch<ExtForeignToplevelListV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.windows.entry(toplevel.id()).or_default();
            // Ask for the cosmic extension object so we receive its `state` and
            // `geometry` events, and remember which ext handle it maps back to.
            if let Some(info) = &state.info {
                let cosmic = info.get_cosmic_toplevel(&toplevel, qh, ());
                state.cosmic_to_ext.insert(cosmic.id(), toplevel.id());
            }
            state.dirty = true;
        }
    }

    event_created_child!(AppState, ExtForeignToplevelListV1, [
        EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let window = state.windows.entry(proxy.id()).or_default();
        match event {
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                window.app_id = Some(app_id);
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                window.title = Some(title);
            }
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                window.identifier = Some(identifier);
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                state.windows.remove(&proxy.id());
            }
            _ => {}
        }
        state.dirty = true;
    }
}

impl Dispatch<ZcosmicToplevelInfoV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ZcosmicToplevelInfoV1,
        event: zcosmic_toplevel_info_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // `done` is emitted once the current atomic batch of toplevel/state
        // changes has been fully sent. The first one signals that every handle's
        // initial `state` event has arrived.
        if let zcosmic_toplevel_info_v1::Event::Done = event {
            state.done = true;
        }
    }

    // Deprecated since v2 (we bind v2+, so this never fires), but the interface
    // declares a child-creating event so it must be registered.
    event_created_child!(AppState, ZcosmicToplevelInfoV1, [
        zcosmic_toplevel_info_v1::EVT_TOPLEVEL_OPCODE => (ZcosmicToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZcosmicToplevelHandleV1, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let cosmic_id = proxy.id();
        match event {
            zcosmic_toplevel_handle_v1::Event::State { state: states } => {
                // `states` is a packed array of u32 values in native byte order.
                let decoded: Vec<u32> = states
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                if let Some(window) = state.window_for_cosmic(&cosmic_id) {
                    window.states = decoded;
                }
                state.dirty = true;
            }
            zcosmic_toplevel_handle_v1::Event::Geometry {
                x,
                y,
                width,
                height,
                ..
            } => {
                if let Some(window) = state.window_for_cosmic(&cosmic_id) {
                    window.geometry = Some((x, y, width, height));
                }
                state.dirty = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for AppState {
    fn event(
        _state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: wayland_client::WEnum::Value(caps),
        } = event
            && caps.contains(wl_seat::Capability::Pointer)
        {
            // The pointer's events are what tell us where the cursor is.
            seat.get_pointer(qh, ());
        }
    }
}

impl Dispatch<WlPointer, ()> for AppState {
    fn event(
        state: &mut Self,
        _pointer: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.cursor = Some((surface_x, surface_y));
                state.dirty = true;
            }
            wl_pointer::Event::Leave { .. } => {
                state.cursor = None;
                state.dirty = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                if width > 0 && height > 0 {
                    // (Re)allocate a full-screen transparent buffer to match the
                    // configured size, then paint the current highlight into it.
                    ensure_buffer(state, qh, width as i32, height as i32);
                    render(state);
                }
                state.mapped = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                eprintln!("Overlay surface was closed by the compositor; exiting.");
                std::process::exit(0);
            }
            _ => {}
        }
    }
}

// These globals/objects emit no events we care about.
delegate_noop!(AppState: ignore WlCompositor);
delegate_noop!(AppState: ignore WlShm);
delegate_noop!(AppState: ignore WlShmPool);
delegate_noop!(AppState: ignore WlBuffer);
delegate_noop!(AppState: ignore WlSurface);
delegate_noop!(AppState: ignore WlOutput);
delegate_noop!(AppState: ignore ZwlrLayerShellV1);

fn main() {
    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("Could not connect to a Wayland compositor: {e}");
            eprintln!("This tool must run inside a Wayland session (e.g. COSMIC).");
            std::process::exit(1);
        }
    };

    let (globals, mut queue) = match registry_queue_init::<AppState>(&conn) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to initialize the Wayland registry: {e}");
            std::process::exit(1);
        }
    };
    let qh = queue.handle();

    let mut state = AppState::default();

    // Bind v2+ so we use the `get_cosmic_toplevel` / `ext_foreign_toplevel_list`
    // flow that carries app_id/title/identifier plus cosmic state & geometry.
    let info = match globals.bind::<ZcosmicToplevelInfoV1, _, _>(&qh, 2..=3, ()) {
        Ok(info) => info,
        Err(e) => {
            eprintln!("zcosmic_toplevel_info_v1 (v2+) is not available: {e}");
            eprintln!("This protocol is COSMIC-specific; are you on the COSMIC desktop?");
            std::process::exit(1);
        }
    };
    state.info = Some(info);

    if let Err(e) = globals.bind::<ExtForeignToplevelListV1, _, _>(&qh, 1..=1, ()) {
        eprintln!("ext_foreign_toplevel_list_v1 is not available: {e}");
        std::process::exit(1);
    }

    // Globals needed to build the fullscreen transparent overlay that lets us
    // read the pointer position.
    let compositor = bind_required::<WlCompositor>(&globals, &qh, "wl_compositor", 1..=6);
    let shm = bind_required::<WlShm>(&globals, &qh, "wl_shm", 1..=1);
    let layer_shell =
        bind_required::<ZwlrLayerShellV1>(&globals, &qh, "zwlr_layer_shell_v1", 1..=4);
    // A seat carries the pointer capability.
    let _seat = bind_required::<WlSeat>(&globals, &qh, "wl_seat", 1..=9);

    state.shm = Some(shm);

    // Bind every wl_output. This is required for geometry: cosmic-comp's
    // toplevel `geometry` event carries a wl_output object argument, so it only
    // emits geometry for outputs the client has actually bound. Without this we
    // never learn any window's position/size.
    let registry = globals.registry();
    for global in globals.contents().clone_list() {
        if global.interface == WlOutput::interface().name {
            let version = global.version.min(4);
            let output: WlOutput = registry.bind(global.name, version, &qh, ());
            state.outputs.push(output);
        }
    }

    // The full-screen buffer is created lazily once the layer surface's first
    // `configure` tells us the output dimensions (see ensure_buffer).

    // Build the overlay: top layer, anchored to every edge, no keyboard grab
    // (so Ctrl-C in the terminal still works), and no exclusive zone.
    let surface = compositor.create_surface(&qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "getappid-inspect".to_string(),
        &qh,
        (),
    );
    layer_surface.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right
            | zwlr_layer_surface_v1::Anchor::Bottom,
    );
    layer_surface.set_exclusive_zone(-1);
    surface.commit();
    state.overlay_surface = Some(surface);
    state.layer_surface = Some(layer_surface);

    // Drain the initial bursts: toplevels + their app_ids/geometry, plus the
    // overlay's first `configure`. The cosmic state/geometry and the info `done`
    // arrive on a later cosmic-comp tick, so loop with short sleeps until we've
    // both mapped the overlay and seen `done` (bounded so we never hang).
    for _ in 0..40 {
        if queue.roundtrip(&mut state).is_err() {
            break;
        }
        if state.done && state.mapped {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }

    eprintln!("Move the mouse to inspect the window underneath. Press Ctrl-C to quit.\n");
    state.dirty = true;

    // Live loop: block until events arrive (pointer motion, window changes),
    // then reprint in place if anything relevant changed.
    loop {
        if queue.blocking_dispatch(&mut state).is_err() {
            break;
        }
        if state.dirty {
            print_report(&state);
            render(&mut state);
            state.dirty = false;
        }
    }
}

/// Bind a required global, exiting with a helpful message if it is missing.
fn bind_required<I>(
    globals: &wayland_client::globals::GlobalList,
    qh: &QueueHandle<AppState>,
    name: &str,
    version: std::ops::RangeInclusive<u32>,
) -> I
where
    I: Proxy + 'static,
    AppState: Dispatch<I, ()>,
{
    match globals.bind::<I, _, _>(qh, version, ()) {
        Ok(proxy) => proxy,
        Err(e) => {
            eprintln!("Required Wayland global '{name}' is not available: {e}");
            std::process::exit(1);
        }
    }
}

/// (Re)allocate the full-screen transparent ARGB8888 buffer backing the overlay
/// so it matches the configured `w`x`h`. Reuses the existing buffer if the size
/// is unchanged. The buffer is mmap'd (MAP_SHARED) so we can repaint into it.
fn ensure_buffer(state: &mut AppState, qh: &QueueHandle<AppState>, w: i32, h: i32) {
    if state.buffer.is_some() && state.screen_w == w && state.screen_h == h {
        return;
    }
    let Some(shm) = state.shm.clone() else {
        return;
    };

    // Drop any previous mapping/pool/buffer before making a new one.
    state.buffer = None;
    state.mmap = None;
    state.pool = None;
    state.shm_file = None;
    state.last_rect = None;
    state.repaint = true;

    let stride = w * 4;
    let size = (stride * h) as u64;

    let mut path = std::env::temp_dir();
    path.push(format!("getappid-shm-{}.bin", std::process::id()));

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to create SHM backing file: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = file.set_len(size) {
        eprintln!("Failed to size SHM backing file: {e}");
        std::process::exit(1);
    }
    let _ = std::fs::remove_file(&path);

    let mmap = match unsafe { MmapOptions::new().len(size as usize).map_mut(&file) } {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to mmap SHM buffer: {e}");
            std::process::exit(1);
        }
    };

    let borrow_fd: BorrowedFd<'_> = unsafe { BorrowedFd::borrow_raw(file.as_raw_fd()) };
    let pool = shm.create_pool(borrow_fd, size as i32, qh, ());
    let buffer = pool.create_buffer(0, w, h, stride, wl_shm::Format::Argb8888, qh, ());

    state.screen_w = w;
    state.screen_h = h;
    state.mmap = Some(mmap);
    state.pool = Some(pool);
    state.shm_file = Some(file);
    state.buffer = Some(buffer);
}

/// Geometry (x, y, w, h) of the window currently under the cursor, if any.
/// Among overlapping candidates, picks the smallest-area one as a best-effort
/// "topmost" guess (z-order isn't exposed). Skips minimized windows.
fn under_cursor_rect(state: &AppState) -> Option<(i32, i32, i32, i32)> {
    let (px, py) = state.cursor?;
    state
        .windows
        .values()
        .filter(|w| !w.has_state(STATE_MINIMIZED) && w.contains(px, py))
        .filter_map(|w| w.geometry)
        .min_by_key(|(_, _, width, height)| (*width as i64) * (*height as i64))
}

/// Paint a border around the window under the cursor into the overlay buffer and
/// commit it. No-op (beyond the first paint) while the highlighted rectangle is
/// unchanged, so moving within one window doesn't keep redrawing.
fn render(state: &mut AppState) {
    let rect = under_cursor_rect(state);
    if rect == state.last_rect && !state.repaint {
        return;
    }
    state.last_rect = rect;
    state.repaint = false;

    let (Some(surface), Some(buffer)) = (
        state.overlay_surface.clone(),
        state.buffer.clone(),
    ) else {
        return;
    };
    let (sw, sh) = (state.screen_w, state.screen_h);
    let Some(mmap) = state.mmap.as_mut() else {
        return;
    };

    // Reinterpret the byte buffer as native-endian u32 pixels.
    let pixels: &mut [u32] = as_u32_pixels(mmap);

    // Clear to fully transparent.
    for px in pixels.iter_mut() {
        *px = 0;
    }

    if let Some((x, y, w, h)) = rect {
        let t = BORDER_THICKNESS;
        // Four bars making up the outline, each clamped to the screen.
        fill_rect(pixels, sw, sh, x, y, w, t, BORDER_COLOR); // top
        fill_rect(pixels, sw, sh, x, y + h - t, w, t, BORDER_COLOR); // bottom
        fill_rect(pixels, sw, sh, x, y, t, h, BORDER_COLOR); // left
        fill_rect(pixels, sw, sh, x + w - t, y, t, h, BORDER_COLOR); // right
    }

    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, sw, sh);
    surface.commit();
}

/// Fill an axis-aligned rectangle in `pixels` (a `sw`x`sh` grid), clamped to the
/// buffer bounds.
fn fill_rect(pixels: &mut [u32], sw: i32, sh: i32, x: i32, y: i32, w: i32, h: i32, color: u32) {
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(sw);
    let y1 = (y + h).min(sh);
    for yy in y0..y1 {
        let row = (yy * sw) as usize;
        for xx in x0..x1 {
            pixels[row + xx as usize] = color;
        }
    }
}

/// Reinterpret a `[u8]` mmap as a `[u32]` pixel slice. The mmap is page-aligned
/// (so suitably aligned for u32) and its length is a multiple of 4.
fn as_u32_pixels(bytes: &mut [u8]) -> &mut [u32] {
    let len = bytes.len() / 4;
    unsafe { std::slice::from_raw_parts_mut(bytes.as_mut_ptr() as *mut u32, len) }
}

fn print_report(state: &AppState) {
    let mut out = String::new();
    // Clear screen (incl. scrollback) and move the cursor home for in-place
    // updates.
    out.push_str("\x1b[2J\x1b[3J\x1b[H");

    match state.cursor {
        None => {
            out.push_str("Pointer is not over the overlay (off-screen?).\n");
        }
        Some((px, py)) => {
            out.push_str(&format!("Cursor at ({:.0}, {:.0})\n\n", px, py));

            // Among every window whose rectangle contains the cursor, pick the
            // smallest-area one as a best-effort "topmost" guess (z-order isn't
            // exposed). Skip minimized windows — they aren't really visible.
            let under = state
                .windows
                .values()
                .filter(|w| !w.has_state(STATE_MINIMIZED) && w.contains(px, py))
                .min_by_key(|w| match w.geometry {
                    Some((_, _, width, height)) => (width as i64) * (height as i64),
                    None => i64::MAX,
                });

            match under {
                None => {
                    out.push_str("No window under the cursor.\n");
                }
                Some(w) => {
                    out.push_str("Window under cursor:\n");
                    out.push_str(&format!(
                        "    app_id     : {}\n",
                        w.app_id.as_deref().unwrap_or("(unknown)")
                    ));
                    out.push_str(&format!(
                        "    title      : {}\n",
                        w.title.as_deref().unwrap_or("(unknown)")
                    ));
                    out.push_str(&format!(
                        "    identifier : {}\n",
                        w.identifier.as_deref().unwrap_or("(unknown)")
                    ));
                    out.push_str(&format!("    states     : {}\n", w.state_labels()));
                    match w.geometry {
                        Some((x, y, width, height)) => {
                            out.push_str(&format!(
                                "    geometry   : {width}x{height} at ({x}, {y})\n"
                            ));
                        }
                        None => out.push_str("    geometry   : (unknown)\n"),
                    }
                }
            }
        }
    }

    // Diagnostic: list every known window with its raw geometry so we can see
    // whether cosmic-comp is actually reporting positions/sizes.
    out.push_str(&format!("\nAll windows ({}):\n", state.windows.len()));
    let mut windows: Vec<&Window> = state.windows.values().collect();
    windows.sort_by(|a, b| a.app_id.cmp(&b.app_id));

    // Pre-compute each window's columns, then pad app_id/title to the widest so
    // the geometry column lines up.
    let rows: Vec<(&str, &str, String)> = windows
        .iter()
        .map(|w| {
            let geom = match w.geometry {
                Some((x, y, width, height)) => format!("{width}x{height} at ({x}, {y})"),
                None => "(unknown)".to_string(),
            };
            (
                w.app_id.as_deref().unwrap_or("(unknown)"),
                w.title.as_deref().unwrap_or("(unknown)"),
                geom,
            )
        })
        .collect();
    let app_w = rows
        .iter()
        .map(|(a, _, _)| a.len())
        .chain([("app_id").len()])
        .max()
        .unwrap_or(0);
    let title_w = rows
        .iter()
        .map(|(_, t, _)| t.len())
        .chain([("title").len()])
        .max()
        .unwrap_or(0);
    let geom_w = rows
        .iter()
        .map(|(_, _, g)| g.len())
        .chain([("geometry").len()])
        .max()
        .unwrap_or(0);

    // Header row + a dashed separator line underneath.
    out.push_str(&format!(
        "    {:<app_w$}  {:<title_w$}  {:<geom_w$}\n",
        "app_id", "title", "geometry"
    ));
    out.push_str(&format!(
        "    {}  {}  {}\n",
        "-".repeat(app_w),
        "-".repeat(title_w),
        "-".repeat(geom_w)
    ));
    for (app_id, title, geom) in &rows {
        out.push_str(&format!("    {app_id:<app_w$}  {title:<title_w$}  {geom}\n"));
    }

    out.push_str("\n(Press Ctrl-C to quit.)\n");

    print!("{out}");
    let _ = std::io::stdout().flush();
}
