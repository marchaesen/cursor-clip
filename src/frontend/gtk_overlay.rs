use crate::shared::config::{PopupPosition, UserConfig, config_path};
use crate::shared::{ClipboardContentType, ClipboardItemPreview};
use cursor_clip::ipc_client::{FrontendClient, PersistenceUpdateError};
use gtk4::prelude::*;
use gtk4::{
    Align, Application, Box, Button, CheckButton, Label, Orientation, Overlay, Revealer,
    SearchEntry,
};
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use libadwaita::{self as adw, prelude::*};
use log::{debug, error, info, warn};
use std::cell::{Cell, RefCell};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::rc::Rc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::frontend::hyprland;
use crate::frontend::toggle::ToggleServer;
use std::os::fd::AsRawFd;

use super::OverlayLocation;

static INIT: Once = Once::new();
pub static CLOSE_REQUESTED: AtomicBool = AtomicBool::new(false);

const OVERLAY_MIN_WIDTH: i32 = 200;
/// Smallest list height a drag on a resize grip can shrink the list to.
const MIN_LIST_HEIGHT: i32 = 120;
/// The card is undecorated, so resizing runs through our own grips.
const RESIZE_HANDLE_THICKNESS: i32 = 6;
const RESIZE_CORNER_SIZE: i32 = 14;

/// Resource path the bundled icons are registered under, and the pin they hold.
/// Shipping the pin ourselves keeps the pinned state looking identical on every
/// desktop rather than depending on whichever icon theme is installed.
const BUNDLED_ICON_RESOURCE_PATH: &str = "/io/github/sirulex/cursor-clip/icons";
const PIN_ICON: &str = "cursor-clip-pin-symbolic";
/// The wastebin used by the per-row delete button and the "Clear All" button.
const TRASH_ICON: &str = "cursor-clip-trash-symbolic";
const PIN_ICON_PIXEL_SIZE: i32 = 16;
/// A red bar drawn diagonally across the pin marks an unpinned row.
const PIN_SLASH_LENGTH: i32 = 16;
const PIN_SLASH_THICKNESS: i32 = 2;
/// Approximate header bar height, used to keep its buttons out of the resize strip.
const HEADER_BAR_HEIGHT: i32 = 42;

// Thread-local storage for the overlay state since GTK objects aren't Send/Sync
thread_local! {
    static OVERLAY_WINDOW: RefCell<Option<adw::ApplicationWindow>> = const { RefCell::new(None) };
    static OVERLAY_APP: RefCell<Option<Application>> = const { RefCell::new(None) };
    // Set when the overlay stays alive between uses (hidden instead of quit).
    static RESIDENT: RefCell<Option<ResidentOverlay>> = const { RefCell::new(None) };
    static PERSISTENCE_PENDING: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone)]
struct HistoryListState {
    // Ordered like the list rows; search hides rows through the filter instead
    // of rebuilding them, so built rows and their layout stay alive.
    all_items: Rc<RefCell<Vec<ClipboardItemPreview>>>,
    search_query: Rc<RefCell<String>>,
    show_trash: Rc<RefCell<bool>>,
    show_pin: Rc<RefCell<bool>>,
    // Bumped on every rebuild so an unfinished incremental fill stops.
    render_generation: Rc<Cell<u64>>,
    // A restarted daemon can reuse IDs while the resident window is hidden.
    current_snapshot: Rc<Cell<bool>>,
    placeholder: Label,
}

impl HistoryListState {
    fn can_act_on_rows(&self) -> bool {
        self.current_snapshot.get() && !persistence_pending()
    }
}

struct PersistenceControls {
    check: CheckButton,
    spinner: gtk4::Spinner,
    status: Label,
    list_box: gtk4::ListBox,
    clear_button: Button,
    history_state: HistoryListState,
    history_status: Label,
    refresh_generation: Cell<u64>,
    confirmed: Cell<Option<bool>>,
    checking: Cell<bool>,
}

struct OverlayContent {
    persistence: Rc<PersistenceControls>,
    overlay: Overlay,
    list_size: Rc<ListSize>,
    title_handle: Box,
    list_box: gtk4::ListBox,
    history_state: HistoryListState,
    search_entry: SearchEntry,
    menu_revealer: Revealer,
    resize_handles: Vec<ResizeHandle>,
}

/// A drag strip that resizes the card along the given axes.
struct ResizeHandle {
    widget: Box,
    horizontal: bool,
    vertical: bool,
}

/// The built overlay: window plus the parts that are reset on every show.
struct OverlayUi {
    window: adw::ApplicationWindow,
    backdrop: Overlay,
    list_size: Rc<ListSize>,
    list_box: gtk4::ListBox,
    history_state: HistoryListState,
    search_entry: SearchEntry,
    menu_revealer: Revealer,
    placement: Rc<Cell<CardPlacement>>,
}

/// A hidden overlay kept in memory with its rows already laid out, so the
/// next open only has to apply history changes and map the window.
struct ResidentOverlay {
    ui: OverlayUi,
    _hold: gtk4::gio::ApplicationHoldGuard,
}

fn persistence_pending() -> bool {
    PERSISTENCE_PENDING.with(Cell::get)
}

fn open_history(controls: &Rc<PersistenceControls>) {
    controls.history_state.current_snapshot.set(false);
    let list = &controls.list_box;
    // Start on the entry that is actually in the clipboard.
    //
    // Focus goes on the row, not on the list: GtkListBox answers the arrow keys
    // itself, from the row holding focus, and a list focused on itself has no
    // such row and so ignores them. With no entry in the clipboard nothing is
    // selected or focused - focusing a populated list then would hand it a
    // cursor row and put a highlight back on the first entry.
    select_initial_row(list, &controls.history_state, true);
    if !list_rows(list).iter().any(is_row_shown)
        && let Some(root) = list.root()
    {
        // An empty ListBox has no child for its grab_focus implementation.
        // Keep logical focus here until the first asynchronous snapshot arrives.
        root.set_focus(Some(list));
    }
    if !persistence_pending() {
        update_persistence(controls.clone(), None);
        refresh_history(controls.clone());
    }
}

fn refresh_history(controls: Rc<PersistenceControls>) {
    let generation = controls.refresh_generation.get().wrapping_add(1);
    controls.refresh_generation.set(generation);
    gtk4::glib::spawn_future_local(async move {
        loop {
            let rendered = controls.history_state.render_generation.get();
            let result = FrontendClient::history().await;
            if controls.refresh_generation.get() != generation || persistence_pending() {
                return;
            }
            match result {
                Ok(_) if rendered != controls.history_state.render_generation.get() => {
                    // A pin, delete or expiry changed the list after this request started.
                    continue;
                }
                Ok(items) => {
                    apply_history(&controls.list_box, &controls.history_state, items);
                    controls.history_state.current_snapshot.set(true);
                    controls.history_status.set_visible(false);
                }
                Err(error) => {
                    warn!("Failed to refresh clipboard history: {error}");
                    controls
                        .history_status
                        .set_label("Could not refresh clipboard history. Reopen to retry.");
                    controls.history_status.set_visible(true);
                }
            }
            break;
        }
    });
}

fn apply_history(list: &gtk4::ListBox, state: &HistoryListState, items: Vec<ClipboardItemPreview>) {
    let focused = list.root().and_then(|root| root.focus());
    let focus_row = focused.as_ref().is_some_and(|widget| {
        widget == list.upcast_ref::<gtk4::Widget>()
            || widget.is::<gtk4::ListBoxRow>() && widget.is_ancestor(list)
    });
    // On reopen the first row should be the newest item in the new snapshot.
    // During an in-place refresh, keep the user's existing selection.
    let selected = state
        .current_snapshot
        .get()
        .then(|| selected_item_id(list))
        .flatten();
    *state.all_items.borrow_mut() = items;
    rebuild_list(list, state);
    if let Some(row) = selected.and_then(|id| {
        list_rows(list)
            .into_iter()
            .find(|row| row_item_id(row) == Some(id) && is_row_shown(row))
    }) {
        list.select_row(Some(&row));
    }
    if focus_row && let Some(row) = list.selected_row() {
        row.grab_focus();
    }
}

// A read-only query leaves keyboard navigation and shutdown available.
// Only a user-requested transition gates actions that need the backend lock.
fn update_persistence(controls: Rc<PersistenceControls>, enabled: Option<bool>) {
    if controls.checking.replace(true) {
        return;
    }
    let changing = enabled.is_some();
    if changing {
        PERSISTENCE_PENDING.with(|pending| pending.set(true));
        controls.list_box.set_sensitive(false);
        controls.clear_button.set_sensitive(false);
        controls
            .refresh_generation
            .set(controls.refresh_generation.get().wrapping_add(1));
    }
    controls.check.set_sensitive(false);
    controls.status.remove_css_class("error");
    controls.status.set_label(if changing {
        "Updating history storage…"
    } else {
        "Checking history storage…"
    });
    controls.status.set_visible(true);
    controls.spinner.set_visible(true);
    controls.spinner.start();

    gtk4::glib::spawn_future_local(async move {
        match FrontendClient::persistence(enabled).await {
            Ok(enabled) => {
                controls.confirmed.set(Some(enabled));
                controls.check.set_inconsistent(false);
                controls.check.set_active(enabled);
                controls.status.set_visible(false);
            }
            Err(error) => {
                let message = match error {
                    PersistenceUpdateError::Rejected(error) if changing => {
                        warn!("Daemon rejected persistence change: {error}");
                        if let Some(previous) = controls.confirmed.get() {
                            controls.check.set_active(previous);
                        }
                        "Could not change history storage."
                    }
                    PersistenceUpdateError::Rejected(error)
                    | PersistenceUpdateError::Unconfirmed(error) => {
                        warn!("Could not confirm history storage: {error}");
                        controls.confirmed.set(None);
                        if changing {
                            controls.history_state.current_snapshot.set(false);
                        }
                        "Could not confirm history storage. Reopen to check again."
                    }
                };
                controls
                    .check
                    .set_inconsistent(controls.confirmed.get().is_none());
                controls.status.set_label(message);
                controls.status.add_css_class("error");
            }
        }
        controls.spinner.stop();
        controls.spinner.set_visible(false);
        controls.checking.set(false);
        controls
            .check
            .set_sensitive(controls.confirmed.get().is_some());
        if changing {
            PERSISTENCE_PENDING.with(|pending| pending.set(false));
            controls.list_box.set_sensitive(true);
            controls.clear_button.set_sensitive(true);
            if is_close_requested() {
                request_quit();
            } else {
                refresh_history(controls);
            }
        }
    });
}

pub(super) fn overlay_config(path: &Path) -> Option<UserConfig> {
    match UserConfig::load_or_create(path) {
        Ok(config) => Some(config),
        Err(e) => {
            eprintln!(
                "cursor-clip: cannot load {}. Using temporary display settings until the file is corrected.\n{e}",
                path.display()
            );
            None
        }
    }
}

pub fn is_close_requested() -> bool {
    CLOSE_REQUESTED.load(Ordering::Relaxed)
}

pub fn reset_close_flags() {
    CLOSE_REQUESTED.store(false, Ordering::Relaxed);
}

// Centralized quit path to avoid double-close reentrancy and ensure flags + app quit
fn request_quit() {
    // A resident overlay is only hidden; it stays ready for the next open.
    if RESIDENT.with(|resident| resident.borrow().is_some()) {
        hide_resident_overlay();
        return;
    }

    CLOSE_REQUESTED.store(true, Ordering::Relaxed);
    if persistence_pending() {
        OVERLAY_WINDOW.with(|window| {
            if let Some(window) = window.borrow().as_ref() {
                window.set_visible(false);
            }
        });
        return;
    }
    // Prefer quitting the application (cleaner teardown) over closing the window directly
    OVERLAY_APP.with(|a| {
        if let Some(ref app) = *a.borrow() {
            app.quit();
        }
    });

    // Fallback: close the window if app is unavailable
    OVERLAY_WINDOW.with(|w| {
        if let Some(ref win) = *w.borrow() {
            win.close();
        }
    });
}

/// Run the overlay. With `resident`, closing hides the window and later
/// toggle requests show it again instead of starting a new process.
pub(super) fn init_clipboard_overlay(
    location: OverlayLocation,
    toggle_listener: UnixListener,
    resident: bool,
    config: UserConfig,
) -> Result<(), std::boxed::Box<dyn std::error::Error + Send + Sync>> {
    INIT.call_once(|| {
        adw::init().expect("Failed to initialize libadwaita");
        register_bundled_icons();
    });
    configure_color_scheme();

    // Create the application (was returned from init_application())
    let app: Application = adw::Application::builder()
        .application_id("io.github.sirulex.cursor-clip")
        .build()
        .upcast();

    let app_clone = app.clone();
    app.connect_activate(move |_| {
        // A toggle can arrive after pointer capture but just before GTK starts
        // dispatching. Do not briefly show a window in that race.
        if is_close_requested() {
            app_clone.quit();
            return;
        }

        let ui = create_layer_shell_window(&app_clone, location.clone(), config.clone());
        let window = ui.window.clone();

        // Store the window in our thread-local storage
        OVERLAY_WINDOW.with(|w| {
            *w.borrow_mut() = Some(window.clone());
        });

        OVERLAY_APP.with(|a| {
            *a.borrow_mut() = Some(app_clone.clone());
        });

        window.present();
        if resident {
            RESIDENT.with(|slot| {
                *slot.borrow_mut() = Some(ResidentOverlay {
                    ui,
                    _hold: app_clone.hold(),
                });
            });
        }

        debug!("Libadwaita overlay window created");
    });

    // Keep listening while GTK owns the main loop. A new cursor-clip-toggle process
    // connects here: a resident overlay toggles its visibility, otherwise the
    // overlay quits instead of queueing another. The wait happens on a helper
    // thread so an idle overlay never wakes up.
    std::thread::spawn(move || {
        loop {
            let mut poll_fd = libc::pollfd {
                fd: toggle_listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll_fd is a single initialized pollfd that outlives the call.
            if unsafe { libc::poll(&mut poll_fd, 1, -1) } < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            let requests = match ToggleServer::count_toggle_requests_from(&toggle_listener) {
                Ok(requests) => requests,
                Err(error) => {
                    warn!("Failed to receive overlay toggle request: {error}");
                    usize::from(!resident)
                }
            };
            if requests == 0 {
                continue;
            }
            if !resident {
                gtk4::glib::MainContext::default().invoke(request_quit);
                break;
            }
            // Every request flips visibility; two quick presses cancel out.
            if requests % 2 == 1 {
                gtk4::glib::MainContext::default().invoke(toggle_resident_overlay);
            }
        }
    });

    // Run the application
    app.run_with_args::<String>(&[]);

    // Belt-and-suspenders: clear TLS after run returns
    RESIDENT.with(|slot| {
        slot.borrow_mut().take();
    });
    OVERLAY_WINDOW.with(|w| {
        *w.borrow_mut() = None;
    });
    OVERLAY_APP.with(|a| {
        *a.borrow_mut() = None;
    });
    Ok(())
}

fn configure_color_scheme() {
    let style_manager = adw::StyleManager::default();
    style_manager.set_color_scheme(adw::ColorScheme::Default);
}

/// Create and configure the sync layer shell window
fn create_layer_shell_window(
    app: &Application,
    location: OverlayLocation,
    config: UserConfig,
) -> OverlayUi {
    // Create the main window using Adwaita ApplicationWindow
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Clipboard History")
        .decorated(false)
        .build();
    window.add_css_class("cursor-clip-backdrop");

    // Initialize layer shell for this window
    window.init_layer_shell();

    // Configure layer shell properties
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some("cursor-clip"));

    // Cover the output so a click outside the card can close the overlay.
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    if let Some(gdk_monitor) = location.monitor.as_deref().and_then(find_gdk_monitor) {
        window.set_monitor(Some(&gdk_monitor));
    }

    window.set_exclusive_zone(-1);

    // Make window keyboard interactive
    window.set_keyboard_mode(gtk4_layer_shell::KeyboardMode::Exclusive);

    // Apply custom styling
    apply_custom_styling(&window);

    // Create and set content (also obtain list_box for navigation)
    let placement = Rc::new(Cell::new(CardPlacement {
        position: config.position,
        margin: config.margin,
        cursor: location.cursor,
        offset: (0, 0),
        pinned: None,
    }));
    let content = generate_overlay_content(config);
    let card = content.overlay.clone();
    card.add_css_class("cursor-clip-card");
    card.set_overflow(gtk4::Overflow::Hidden);

    let backdrop = create_backdrop(&card, placement.clone());
    window.set_content(Some(&backdrop));
    content.list_size.apply(
        location.monitor_width,
        location.monitor_height,
        placement.get(),
    );
    attach_resize_gestures(&backdrop, &card, &content, &placement, &config_path());
    attach_move_gesture(&backdrop, &card, &content, &placement);
    content.search_entry.connect_visible_notify({
        let size = Rc::downgrade(&content.list_size);
        let window = window.downgrade();
        let placement = placement.clone();
        move |_| {
            if let (Some(size), Some(window)) = (size.upgrade(), window.upgrade())
                && window.is_mapped()
            {
                size.apply(window.width(), window.height(), placement.get());
            }
        }
    });
    window.connect_realize({
        let size = content.list_size.clone();
        let placement = placement.clone();
        move |window| {
            if let Some(surface) = window.surface() {
                let size = Rc::downgrade(&size);
                let placement = placement.clone();
                surface.connect_layout(move |_, width, height| {
                    if let Some(size) = size.upgrade() {
                        size.apply(width, height, placement.get());
                    }
                });
            }
        }
    });

    let backdrop_click = gtk4::GestureClick::new();
    backdrop_click.set_button(0);
    backdrop_click.connect_pressed({
        let card_widget: gtk4::Widget = card.clone().upcast();
        move |gesture, _, click_x, click_y| {
            let Some(widget) = gesture.widget() else {
                return;
            };
            // Let the modal dialog handle clicks, including its backdrop.
            if widget
                .downcast_ref::<adw::ApplicationWindow>()
                .is_some_and(|window| window.visible_dialog().is_some())
            {
                return;
            }
            let inside_card = widget
                .pick(click_x, click_y, gtk4::PickFlags::DEFAULT)
                .is_some_and(|picked| picked == card_widget || picked.is_ancestor(&card_widget));
            if !inside_card {
                debug!("Click outside the overlay card - closing");
                request_quit();
            }
        }
    });
    window.add_controller(backdrop_click);

    // Esc/arrows/Enter/Delete handling and type-to-search
    let key_controller = generate_key_controller(
        &content.list_box,
        &content.history_state,
        &content.search_entry,
    );
    window.add_controller(key_controller);

    // Start in the history list; typing reveals and focuses search on demand.
    window.connect_map({
        let controls = content.persistence;
        move |_| {
            open_history(&controls);
        }
    });

    // Add close request handler to ensure any window close goes through our logic
    window.connect_close_request(|_window| {
        debug!("Window close requested - closing overlay and capture layer");
        request_quit();
        // Stop default handler to avoid double-close reentrancy during teardown
        gtk4::glib::Propagation::Stop
    });

    OverlayUi {
        window,
        backdrop,
        list_size: content.list_size,
        list_box: content.list_box,
        history_state: content.history_state,
        search_entry: content.search_entry,
        menu_revealer: content.menu_revealer,
        placement,
    }
}

fn toggle_resident_overlay() {
    let visible = RESIDENT.with(|resident| {
        resident
            .borrow()
            .as_ref()
            .is_some_and(|resident| resident.ui.window.is_visible())
    });
    if visible {
        hide_resident_overlay();
    } else {
        show_resident_overlay();
    }
}

fn hide_resident_overlay() {
    RESIDENT.with(|resident| {
        if let Some(resident) = resident.borrow().as_ref() {
            if let Some(dialog) = resident.ui.window.visible_dialog() {
                dialog.force_close();
            }
            resident.ui.window.set_visible(false);
        }
    });
}

/// Re-open the kept overlay: apply history changes to the existing rows,
/// reset search/menu/scroll and place the card on the current output.
fn show_resident_overlay() {
    RESIDENT.with(|resident| {
        let resident = resident.borrow();
        let Some(resident) = resident.as_ref() else {
            return;
        };
        let ui = &resident.ui;
        let mut placement = ui.placement.get();
        if let Some(config) = overlay_config(&config_path()) {
            ui.list_size.set_requested(config.width, config.height);
            placement.position = config.position;
            placement.margin = config.margin;
        }
        // A drag only lasts for the show it was made in.
        placement.offset = (0, 0);
        placement.pinned = None;

        ui.history_state.search_query.borrow_mut().clear();
        ui.search_entry.set_text("");
        ui.search_entry.set_visible(false);
        ui.menu_revealer.set_reveal_child(false);
        ui.menu_revealer.set_visible(false);
        rebuild_list(&ui.list_box, &ui.history_state);
        refresh_time_labels(&ui.list_box, &ui.history_state);
        if let Some(scrolled) = ui
            .list_box
            .ancestor(gtk4::ScrolledWindow::static_type())
            .and_then(|widget| widget.downcast::<gtk4::ScrolledWindow>().ok())
        {
            scrolled.vadjustment().set_value(0.0);
        }

        let mut monitor_width = ui.window.width();
        let mut monitor_height = ui.window.height();
        if let Some(location) = hyprland::overlay_location(placement.position) {
            let monitor = location.monitor.as_deref().and_then(find_gdk_monitor);
            ui.window.set_monitor(monitor.as_ref());
            placement.cursor = location.cursor;
            monitor_width = location.monitor_width;
            monitor_height = location.monitor_height;
        } else if placement.position != PopupPosition::Cursor {
            ui.window.set_monitor(None);
        }

        ui.list_size.apply(monitor_width, monitor_height, placement);
        ui.placement.set(placement);
        ui.backdrop.queue_allocate();
        ui.window.present();
    });
}

fn find_gdk_monitor(connector: &str) -> Option<gtk4::gdk::Monitor> {
    let display = gtk4::gdk::Display::default()?;
    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|index| monitors.item(index))
        .filter_map(|item| item.downcast::<gtk4::gdk::Monitor>().ok())
        .find(|monitor| monitor.connector().as_deref() == Some(connector))
}

struct ListSize {
    header: adw::HeaderBar,
    scrolled: gtk4::ScrolledWindow,
    requested: Cell<(i32, i32)>,
    last_width: Cell<Option<i32>>,
}

impl ListSize {
    fn set_requested(&self, width: i32, height: i32) {
        self.requested.set((width, height));
    }

    fn header_minimum(&self) -> i32 {
        self.header.measure(Orientation::Horizontal, -1).0
    }

    /// Size the list for an output of `area_width` × `area_height`. A
    /// dimension of 0 means the output size is not known yet.
    fn apply(&self, area_width: i32, area_height: i32, placement: CardPlacement) {
        let (requested_width, requested_height) = self.requested.get();
        let header_min = self.header_minimum();
        let minimum = header_min.max(OVERLAY_MIN_WIDTH);
        // Only the output limits the width, as it does the height below. The
        // card CSS adds a one-pixel border on each side of the list.
        let max_width = if area_width > 0 {
            (placement.available(area_width) - 2).max(minimum)
        } else {
            i32::MAX
        };
        let width = requested_width.min(max_width).max(minimum);
        if requested_width < minimum
            && (self.last_width.get() != Some(requested_width)
                || self.scrolled.min_content_width() != width)
        {
            eprintln!(
                "cursor-clip: configured width {requested_width} is too small (header minimum: {header_min} logical pixels). Using the minimum viable width of {minimum} logical pixels."
            );
        }
        self.last_width.set(Some(requested_width));
        self.scrolled.set_min_content_width(width);
        // The vertical box also contains the header and optional search field.
        let chrome_height = self.scrolled.parent().map_or(0, |parent| {
            parent.measure(Orientation::Vertical, width).1
                - self.scrolled.measure(Orientation::Vertical, width).1
        });
        let max_height = if area_height > 0 {
            (placement.available(area_height) - chrome_height - 2).max(1)
        } else {
            crate::shared::config::DEFAULT_LIST_HEIGHT
        };
        self.scrolled
            .set_min_content_height(requested_height.clamp(1, max_height));
    }
}

#[derive(Clone, Copy)]
struct CardPlacement {
    position: PopupPosition,
    margin: i32,
    cursor: (f64, f64),
    /// How far the user dragged the card away from its anchored origin.
    /// Reset every time the overlay is shown.
    offset: (i32, i32),
    /// Top-left the card keeps while it is being resized. The anchored origin
    /// depends on the card's size, so a card pushed against an output edge
    /// would slide as it changes size and its dragged border could not follow
    /// the pointer. Folded into `offset` when the card is moved, and reset
    /// every time the overlay is shown.
    pinned: Option<(i32, i32)>,
}

impl CardPlacement {
    fn available(self, extent: i32) -> i32 {
        if self.position == PopupPosition::Center {
            extent
        } else {
            (extent - self.margin.saturating_mul(2)).max(0)
        }
    }

    /// Top-left and bottom-right corners of the space the card's origin may use.
    fn bounds(self, free_width: i32, free_height: i32) -> ((i32, i32), (i32, i32)) {
        let left = self.margin.min(free_width);
        let top = self.margin.min(free_height);
        ((left, top), (free_width - left, free_height - top))
    }

    /// Where the configured position puts the card before any dragging.
    fn anchor(self, free_width: i32, free_height: i32) -> (i32, i32) {
        let ((left, top), (right, bottom)) = self.bounds(free_width, free_height);
        match self.position {
            PopupPosition::Cursor => (
                (self.cursor.0 as i32).clamp(left, right.max(left)),
                (self.cursor.1 as i32).clamp(top, bottom.max(top)),
            ),
            PopupPosition::Center => (free_width / 2, free_height / 2),
            PopupPosition::TopLeft => (left, top),
            PopupPosition::TopRight => (right, top),
            PopupPosition::BottomLeft => (left, bottom),
            PopupPosition::BottomRight => (right, bottom),
        }
    }

    /// Limit a drag offset so the dragged card stays inside the margins.
    fn clamp_offset(self, free_width: i32, free_height: i32, wanted: (i32, i32)) -> (i32, i32) {
        let (x, y) = self.anchor(free_width, free_height);
        let ((left, top), (right, bottom)) = self.bounds(free_width, free_height);
        (
            (x + wanted.0).clamp(left, right.max(left)) - x,
            (y + wanted.1).clamp(top, bottom.max(top)) - y,
        )
    }

    fn origin(self, free_width: i32, free_height: i32) -> (i32, i32) {
        if let Some((x, y)) = self.pinned {
            let ((left, top), (right, bottom)) = self.bounds(free_width, free_height);
            return (
                x.clamp(left, right.max(left)),
                y.clamp(top, bottom.max(top)),
            );
        }
        let (x, y) = self.anchor(free_width, free_height);
        if self.offset == (0, 0) {
            return (x, y);
        }
        let (dx, dy) = self.clamp_offset(free_width, free_height, self.offset);
        (x + dx, y + dy)
    }
}

fn create_backdrop(card: &Overlay, placement: Rc<Cell<CardPlacement>>) -> Overlay {
    let backdrop = Overlay::new();
    // Allocate the card within the output size without adding its position
    // to the window's size request.
    backdrop.add_overlay(card);
    backdrop.connect_get_child_position(move |backdrop, card| {
        let placement = placement.get();
        // The list's minimum content width sets the card's width, and `ListSize`
        // already keeps that within the output. The header's natural width
        // (title plus buttons) is larger and would stop the card from shrinking
        // to a narrow list.
        let width = card
            .measure(Orientation::Horizontal, -1)
            .0
            .min(backdrop.width());
        let (minimum_height, natural_height, _, _) = card.measure(Orientation::Vertical, width);
        let height = natural_height
            .min(placement.available(backdrop.height()).max(minimum_height))
            .min(backdrop.height());
        let (left, top) = placement.origin(backdrop.width() - width, backdrop.height() - height);
        Some(gtk4::gdk::Rectangle::new(left, top, width, height))
    });
    backdrop
}

fn append_settings_toggle(menu: &Box, title: &str, active: bool) -> (Box, CheckButton) {
    let row = Box::new(Orientation::Horizontal, 8);
    row.add_css_class("settings-row");
    let label = Label::new(Some(title));
    label.set_halign(Align::Start);
    label.set_hexpand(true);
    let check = CheckButton::new();
    check.set_active(active);
    label.set_mnemonic_widget(Some(&check));
    row.append(&label);
    row.append(&check);
    menu.append(&row);
    (row, check)
}

fn generate_overlay_content(config: UserConfig) -> OverlayContent {
    // Main container with standard libadwaita spacing
    let main_box = Box::new(Orientation::Vertical, 0);

    // Header bar
    let header_bar = adw::HeaderBar::new();
    // An un-ellipsized title pins the card's minimum width and blocks shrinking.
    let title_label = Label::new(Some("Clipboard History"));
    title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    title_label.set_margin_start(30);
    title_label.set_margin_end(30);
    title_label.set_hexpand(true);
    title_label.set_halign(Align::Center);
    // The title fills the space between the header buttons and doubles as the
    // area the card is dragged by.
    let title_handle = Box::new(Orientation::Horizontal, 0);
    title_handle.set_hexpand(true);
    title_handle.set_cursor_from_name(Some("grab"));
    title_handle.append(&title_label);
    header_bar.set_title_widget(Some(&title_handle));
    header_bar.set_centering_policy(adw::CenteringPolicy::Strict);
    // Layer-shell + undecorated windows can render built-in title buttons unreliably.
    // Use an explicit close button styled like a normal Adwaita title button instead.
    header_bar.set_show_end_title_buttons(false);
    header_bar.set_show_start_title_buttons(false);

    let show_trash_default = config.show_trash;
    let show_pin_default = config.show_pin;
    let instant_paste = Rc::new(Cell::new(config.instant_paste));
    let show_trash_state = Rc::new(RefCell::new(show_trash_default));
    let show_pin_state = Rc::new(RefCell::new(show_pin_default));

    // Search and clear are on the left; options and close are on the right.
    let search_button = Button::builder().icon_name("edit-find-symbolic").build();
    search_button.add_css_class("flat");
    search_button.add_css_class("compact-header-action");
    search_button.set_tooltip_text(Some("Search"));

    let three_dot_menu = Button::builder().icon_name("view-more-symbolic").build();
    three_dot_menu.add_css_class("flat");
    three_dot_menu.add_css_class("compact-header-action");
    three_dot_menu.set_tooltip_text(Some("Options"));

    let header_action_group = Box::new(Orientation::Horizontal, 0);
    header_action_group.add_css_class("header-action-group");
    header_action_group.append(&three_dot_menu);

    let close_icon = gtk4::Image::from_icon_name("window-close-symbolic");
    close_icon.set_pixel_size(16);
    close_icon.set_size_request(16, 16);
    close_icon.set_halign(Align::Center);
    close_icon.set_valign(Align::Center);
    close_icon.set_hexpand(true);
    close_icon.set_vexpand(true);

    let close_icon_box = Box::new(Orientation::Horizontal, 0);
    close_icon_box.add_css_class("manual-close-icon");
    close_icon_box.set_size_request(28, 28);
    close_icon_box.set_halign(Align::Center);
    close_icon_box.set_valign(Align::Center);
    close_icon_box.append(&close_icon);

    let close_button = Button::new();
    close_button.set_child(Some(&close_icon_box));
    close_button.add_css_class("flat");
    close_button.add_css_class("manual-close-button");
    close_button.set_size_request(28, 28);
    close_button.set_tooltip_text(Some("Close"));

    let menu_revealer = Revealer::new();
    menu_revealer.set_reveal_child(false);
    menu_revealer.set_visible(false);
    menu_revealer.set_transition_duration(120);
    menu_revealer.set_transition_type(gtk4::RevealerTransitionType::SlideDown);
    menu_revealer.set_halign(Align::End);
    menu_revealer.set_valign(Align::Start);
    menu_revealer.set_margin_top(HEADER_BAR_HEIGHT);
    menu_revealer.set_margin_end(5);
    menu_revealer.add_css_class("menu-revealer");

    let menu_box = Box::new(Orientation::Vertical, 4);
    menu_box.set_margin_top(4);
    menu_box.set_margin_bottom(4);
    menu_box.set_margin_start(5);
    menu_box.set_margin_end(5);

    let toggle_row = Box::new(Orientation::Horizontal, 4);
    toggle_row.add_css_class("settings-row");
    let toggle_label = Label::new(Some("Show delete button"));
    toggle_label.set_halign(Align::Start);
    toggle_label.set_hexpand(true);
    let toggle_check = CheckButton::new();
    toggle_check.set_active(show_trash_default);
    toggle_row.append(&toggle_label);
    toggle_row.append(&toggle_check);
    menu_box.append(&toggle_row);

    let pin_toggle_row = Box::new(Orientation::Horizontal, 4);
    pin_toggle_row.add_css_class("settings-row");
    let pin_toggle_label = Label::new(Some("Show pin icon"));
    pin_toggle_label.set_halign(Align::Start);
    pin_toggle_label.set_hexpand(true);
    let pin_toggle_check = CheckButton::new();
    pin_toggle_check.set_active(show_pin_default);
    pin_toggle_row.append(&pin_toggle_label);
    pin_toggle_row.append(&pin_toggle_check);
    menu_box.append(&pin_toggle_row);

    let persistence_toggle_row = Box::new(Orientation::Horizontal, 4);
    persistence_toggle_row.add_css_class("settings-row");
    let persistence_toggle_label = Label::new(Some("Persistent history"));
    persistence_toggle_label.set_halign(Align::Start);
    persistence_toggle_label.set_hexpand(true);
    let persistence_toggle_check = CheckButton::new();
    persistence_toggle_check.set_inconsistent(true);
    persistence_toggle_row.append(&persistence_toggle_label);
    let persistence_spinner = gtk4::Spinner::new();
    persistence_spinner.set_visible(false);
    persistence_toggle_row.append(&persistence_spinner);
    persistence_toggle_row.append(&persistence_toggle_check);
    menu_box.append(&persistence_toggle_row);

    let (_, instant_paste_toggle_check) =
        append_settings_toggle(&menu_box, "Instant paste", instant_paste.get());
    let (type_in_row, type_in_check) =
        append_settings_toggle(&menu_box, "Type in", config.type_instead_of_paste);
    instant_paste_toggle_check
        .bind_property("active", &type_in_row, "sensitive")
        .sync_create()
        .build();

    let persistence_status = Label::new(None);
    persistence_status.set_halign(Align::Start);
    persistence_status.set_wrap(true);
    persistence_status.set_max_width_chars(26);
    persistence_status.set_visible(false);
    menu_box.append(&persistence_status);
    let history_status = Label::new(None);
    history_status.set_wrap(true);
    history_status.set_max_width_chars(26);
    history_status.set_visible(false);
    menu_box.append(&history_status);

    let menu_scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .propagate_natural_height(true)
        .child(&menu_box)
        .build();
    menu_revealer.set_child(Some(&menu_scroll));
    header_bar.pack_end(&close_button);
    header_bar.pack_end(&header_action_group);

    close_button.connect_clicked(move |_| {
        request_quit();
    });

    let clear_button = Button::builder().icon_name(TRASH_ICON).build();
    clear_button.add_css_class("flat");
    clear_button.add_css_class("compact-header-action");
    clear_button.set_tooltip_text(Some("Clear entire clipboard history"));
    header_bar.pack_start(&clear_button);
    header_bar.pack_start(&search_button);

    main_box.append(&header_bar);

    // Search stays hidden until the search button is clicked or typing starts.
    let search_entry = SearchEntry::new();
    search_entry.set_visible(false);
    search_entry.set_placeholder_text(Some("Type to search"));
    search_entry.set_margin_start(12);
    search_entry.set_margin_end(12);
    search_entry.set_margin_bottom(6);
    search_entry.add_css_class("clipboard-search");
    // Filtering ~100 rows is cheap; react to every keystroke immediately.
    search_entry.set_property("search-delay", 0u32);
    main_box.append(&search_entry);

    let search_entry_for_button = search_entry.clone();
    search_button.connect_clicked(move |_| {
        focus_search_entry(&search_entry_for_button);
        search_entry_for_button.select_region(0, -1);
    });

    // Create scrolled window for the clipboard list
    let scrolled_window = gtk4::ScrolledWindow::new();
    // External hides the horizontal scrollbar without letting row widths size the card.
    scrolled_window.set_policy(gtk4::PolicyType::External, gtk4::PolicyType::Automatic);

    // Create list box for clipboard items
    let list_box = gtk4::ListBox::new();
    // Use custom styling instead of the default boxed-list to create floating cards
    list_box.add_css_class("clipboard-list");
    //list_box.set_margin_top(3);
    list_box.set_margin_bottom(3);
    list_box.set_margin_start(2);
    list_box.set_margin_end(2);
    list_box.set_selection_mode(gtk4::SelectionMode::Single);

    list_box.set_focusable(true);

    let placeholder = Label::new(None);
    placeholder.add_css_class("dim-label");
    placeholder.set_margin_top(10);
    placeholder.set_margin_bottom(10);
    list_box.set_placeholder(Some(&placeholder));

    let history_state = HistoryListState {
        all_items: Rc::new(RefCell::new(Vec::new())),
        search_query: Rc::new(RefCell::new(String::new())),
        show_trash: show_trash_state,
        show_pin: show_pin_state,
        render_generation: Rc::new(Cell::new(0)),
        current_snapshot: Rc::new(Cell::new(false)),
        placeholder,
    };

    let history_state_for_filter = history_state.clone();
    list_box.set_filter_func(move |row| {
        let query = history_state_for_filter
            .search_query
            .borrow()
            .trim()
            .to_lowercase();
        if query.is_empty() {
            return true;
        }
        row_item_id(row).is_none_or(|item_id| {
            history_state_for_filter
                .all_items
                .borrow()
                .iter()
                .find(|item| item.item_id == item_id)
                .is_some_and(|item| item_matches_query(item, &query))
        })
    });

    // rebuild_list already highlights whichever entry is actually in the
    // clipboard (see select_initial_row). Focus is placed when the window is
    // mapped: a widget cannot take focus before it belongs to a window.
    rebuild_list(&list_box, &history_state);

    // Handle item activation (Enter/Space/double-click) instead of mere selection
    let history_state_for_activation = history_state.clone();
    let instant_paste_for_activation = instant_paste.clone();
    list_box.connect_row_activated(move |_, row| {
        if !history_state_for_activation.can_act_on_rows() {
            return;
        }
        let Some(item_id) = row_item_id(row) else {
            return;
        };
        let items = history_state_for_activation.all_items.borrow();
        if let Some(item) = items.iter().find(|item| item.item_id == item_id) {
            let instant_paste = instant_paste_for_activation.get();
            debug!(
                "Activated clipboard item ID {}: {}",
                item.item_id, item.content_preview
            );

            match FrontendClient::new() {
                Ok(mut client) => {
                    if let Err(e) = client.set_clipboard_by_id(item.item_id, instant_paste) {
                        error!("Error setting clipboard by ID: {}", e);
                    } else {
                        info!("Clipboard set by ID: {}", item.item_id);
                        request_quit();
                    }
                }
                Err(e) => {
                    error!("Error creating frontend client: {}", e);
                }
            }
        }
    });

    scrolled_window.set_child(Some(&list_box));
    main_box.append(&scrolled_window);

    set_delete_buttons_visible(&list_box, show_trash_default);
    set_pin_icons_visible(&list_box, show_pin_default);

    let list_box_for_toggle = list_box.clone();
    let history_state_for_toggle = history_state.clone();
    toggle_check.connect_toggled(move |check| {
        let state = check.is_active();
        if let Err(e) = UserConfig::update(&config_path(), |config| config.show_trash = state) {
            warn!("Failed to save config: {e}");
        }
        *history_state_for_toggle.show_trash.borrow_mut() = state;
        set_delete_buttons_visible(&list_box_for_toggle, state);
    });

    let list_box_for_pin_toggle = list_box.clone();
    let history_state_for_pin_toggle = history_state.clone();
    pin_toggle_check.connect_toggled(move |check| {
        let state = check.is_active();
        if let Err(e) = UserConfig::update(&config_path(), |config| config.show_pin = state) {
            warn!("Failed to save config: {e}");
        }
        *history_state_for_pin_toggle.show_pin.borrow_mut() = state;
        set_pin_icons_visible(&list_box_for_pin_toggle, state);
    });

    let persistence_controls = Rc::new(PersistenceControls {
        check: persistence_toggle_check.clone(),
        spinner: persistence_spinner,
        status: persistence_status,
        list_box: list_box.clone(),
        clear_button: clear_button.clone(),
        history_state: history_state.clone(),
        history_status,
        refresh_generation: Cell::new(0),
        confirmed: Cell::new(None),
        checking: Cell::new(false),
    });
    let controls_for_persistence_toggle = Rc::downgrade(&persistence_controls);
    persistence_toggle_check.connect_toggled(move |check| {
        if check.is_sensitive()
            && let Some(controls) = controls_for_persistence_toggle.upgrade()
        {
            update_persistence(controls, Some(check.is_active()));
        }
    });

    instant_paste_toggle_check.connect_toggled(move |check| {
        let state = check.is_active();
        instant_paste.set(state);
        if let Err(e) = UserConfig::update(&config_path(), |config| config.instant_paste = state) {
            warn!("Failed to save config: {}", e);
        }
    });

    let reverting = Cell::new(false);
    let type_in_changed = type_in_check.connect_toggled(move |check| {
        if reverting.get() {
            return;
        }
        let active = check.is_active();
        if let Err(e) = UserConfig::update(&config_path(), |cfg| cfg.type_instead_of_paste = active)
        {
            warn!("Failed to save Type in setting: {e}");
            reverting.set(true);
            check.set_active(!active);
            reverting.set(false);
        }
    });
    // A resident overlay can outlive edits made directly to config.toml.
    menu_box.connect_map(move |_| {
        if let Ok(config) = UserConfig::read(&config_path()) {
            type_in_check.block_signal(&type_in_changed);
            type_in_check.set_active(config.type_instead_of_paste);
            type_in_check.unblock_signal(&type_in_changed);
        }
    });

    let list_box_for_search = list_box.clone();
    let history_state_for_search = history_state.clone();
    search_entry.connect_search_changed(move |entry| {
        *history_state_for_search.search_query.borrow_mut() = entry.text().to_string();
        apply_search_filter(&list_box_for_search, &history_state_for_search);
    });

    let list_box_for_search_activate = list_box.clone();
    search_entry.connect_activate(move |_| {
        if let Some(row) = list_box_for_search_activate.selected_row() {
            row.emit_by_name::<()>("activate", &[]);
        }
    });

    let search_key_controller = gtk4::EventControllerKey::new();
    let list_box_for_search_keys = list_box.clone();
    let search_entry_for_search_keys = search_entry.clone();
    search_key_controller.connect_key_pressed(move |_, key, _, state| {
        use gtk4::gdk::{Key, ModifierType};

        match key {
            Key::u | Key::U
                if state.contains(ModifierType::CONTROL_MASK)
                    && search_entry_for_search_keys.position() > 0 =>
            {
                let cursor = search_entry_for_search_keys.position();
                search_entry_for_search_keys.delete_text(0, cursor);
                search_entry_for_search_keys.set_position(0);
                gtk4::glib::Propagation::Stop
            }
            Key::Down => {
                if select_next_row(&list_box_for_search_keys, true) {
                    gtk4::glib::Propagation::Stop
                } else {
                    gtk4::glib::Propagation::Proceed
                }
            }
            Key::Up => {
                if select_previous_row(&list_box_for_search_keys, true) {
                    gtk4::glib::Propagation::Stop
                } else {
                    gtk4::glib::Propagation::Proceed
                }
            }
            _ => gtk4::glib::Propagation::Proceed,
        }
    });
    search_entry.add_controller(search_key_controller);

    let menu_revealer_toggle = menu_revealer.clone();
    three_dot_menu.connect_clicked(move |_| {
        // Target state, not animation state: is_child_revealed() stays false
        // until the transition finishes, so a second click during the
        // animation would re-open instead of closing.
        let next_state = !menu_revealer_toggle.reveals_child();
        menu_revealer_toggle.set_visible(next_state);
        menu_revealer_toggle.set_reveal_child(next_state);
    });

    let history_for_clear = history_state.clone();
    let list_box_for_clear = list_box.clone();
    let controls_for_clear = persistence_controls.clone();
    clear_button.connect_clicked(move |button| {
        if !history_for_clear.can_act_on_rows() {
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Clear clipboard history?"),
            Some(
                "All unpinned clipboard history items will be deleted. Pinned items are kept. \
                 This cannot be undone.",
            ),
        );
        dialog.add_response("cancel", "_Cancel");
        dialog.add_response("clear", "_Clear");
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
        let history_for_response = history_for_clear.clone();
        let list_box_for_response = list_box_for_clear.clone();
        let controls_for_response = controls_for_clear.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "clear" || !history_for_response.can_act_on_rows() {
                return;
            }
            let mut client = match FrontendClient::new() {
                Ok(client) => client,
                Err(e) => {
                    error!("Error creating frontend client: {}", e);
                    return;
                }
            };

            if let Err(e) = client.clear_history() {
                error!("Error clearing clipboard history: {}", e);
                return;
            }

            // Pinned items survive a clear and the overlay stays open: show
            // that right away, then reload what the backend really kept.
            info!("Clipboard history cleared");
            history_for_response
                .all_items
                .borrow_mut()
                .retain(|item| item.pinned);
            rebuild_list(&list_box_for_response, &history_for_response);
            select_first_row(&list_box_for_response);
            refresh_history(controls_for_response.clone());
        });
        dialog.present(Some(button));
    });

    let resize_right = build_resize_handle("e-resize", Align::End, Align::Fill);
    resize_right.set_size_request(RESIZE_HANDLE_THICKNESS, -1);
    // Start below the header bar so the close and menu buttons stay fully clickable.
    resize_right.set_margin_top(HEADER_BAR_HEIGHT);

    let resize_bottom = build_resize_handle("s-resize", Align::Fill, Align::End);
    resize_bottom.set_size_request(-1, RESIZE_HANDLE_THICKNESS);

    let resize_corner = build_resize_handle("se-resize", Align::End, Align::End);
    resize_corner.set_size_request(RESIZE_CORNER_SIZE, RESIZE_CORNER_SIZE);
    resize_corner.add_css_class("resize-grip");

    let overlay = Overlay::new();
    overlay.set_child(Some(&main_box));
    overlay.add_overlay(&menu_revealer);
    overlay.set_measure_overlay(&menu_revealer, true);
    overlay.add_overlay(&resize_right);
    overlay.add_overlay(&resize_bottom);
    overlay.add_overlay(&resize_corner);

    OverlayContent {
        persistence: persistence_controls,
        overlay,
        list_size: Rc::new(ListSize {
            header: header_bar,
            scrolled: scrolled_window,
            requested: Cell::new((config.width, config.height)),
            last_width: Cell::new(None),
        }),
        title_handle,
        list_box,
        history_state,
        search_entry,
        menu_revealer,
        resize_handles: vec![
            ResizeHandle {
                widget: resize_right,
                horizontal: true,
                vertical: false,
            },
            ResizeHandle {
                widget: resize_bottom,
                horizontal: false,
                vertical: true,
            },
            ResizeHandle {
                widget: resize_corner,
                horizontal: true,
                vertical: true,
            },
        ],
    }
}

/// Create a transparent drag strip used as a resize grip.
fn build_resize_handle(cursor: &str, halign: Align, valign: Align) -> Box {
    let handle = Box::new(Orientation::Horizontal, 0);
    handle.set_halign(halign);
    handle.set_valign(valign);
    handle.set_cursor_from_name(Some(cursor));
    handle
}

/// Where a move drag started: the pointer in backdrop coordinates and the
/// placement offset at that moment.
#[derive(Clone, Copy)]
struct MoveStart {
    pointer: (f64, f64),
    offset: (i32, i32),
}

/// Move the card by dragging the header bar's title area.
///
/// The overlay surface covers the whole output and the card is placed inside
/// it by `CardPlacement`, so the header bar's built-in drag support - which
/// asks the compositor to move a toplevel - does nothing here. The drag is
/// stored as an offset on the placement instead, which is clamped to the
/// output and reset whenever the overlay is shown again.
///
/// Two details keep the card following the cursor:
///
/// The gesture claims its sequence up front. `adw::HeaderBar` wraps itself in a
/// window handle carrying a drag gesture of its own, and that gesture takes the
/// sequence over as soon as the 8px drag threshold is crossed - precisely where
/// the movement otherwise stalls after a few pixels.
///
/// The pointer is tracked in backdrop coordinates. Gesture offsets are relative
/// to the title area, which moves along with the card, so they already have the
/// movement applied so far subtracted from them. The backdrop never moves, so
/// "pointer now minus pointer at drag start" is the true travel.
fn attach_move_gesture(
    backdrop: &Overlay,
    card: &Overlay,
    content: &OverlayContent,
    placement: &Rc<Cell<CardPlacement>>,
) {
    let gesture = gtk4::GestureDrag::new();
    let start: Rc<Cell<Option<MoveStart>>> = Rc::new(Cell::new(None));

    gesture.connect_drag_begin({
        let backdrop = backdrop.clone();
        let card = card.clone();
        let handle = content.title_handle.clone();
        let placement = placement.clone();
        let start = start.clone();
        move |gesture, start_x, start_y| {
            gesture.set_state(gtk4::EventSequenceState::Claimed);
            handle.set_cursor_from_name(Some("grabbing"));
            // A resize pins the card where it is; moving continues from there.
            let mut current = placement.get();
            if current.pinned.is_some()
                && let Some((x, y)) = point_in(&card, &backdrop, 0.0, 0.0)
            {
                let (anchor_x, anchor_y) = current.anchor(
                    (backdrop.width() - card.width()).max(0),
                    (backdrop.height() - card.height()).max(0),
                );
                current.offset = (x.round() as i32 - anchor_x, y.round() as i32 - anchor_y);
                current.pinned = None;
                placement.set(current);
            }
            if let Some(pointer) = point_in(&handle, &backdrop, start_x, start_y) {
                start.set(Some(MoveStart {
                    pointer,
                    offset: placement.get().offset,
                }));
            }
        }
    });

    gesture.connect_drag_update({
        let backdrop = backdrop.clone();
        let card = card.clone();
        let handle = content.title_handle.clone();
        let placement = placement.clone();
        let start = start.clone();
        move |gesture, offset_x, offset_y| {
            let (Some(begin), Some((start_x, start_y))) = (start.get(), gesture.start_point())
            else {
                return;
            };
            if backdrop.width() <= 0 || backdrop.height() <= 0 {
                return;
            }
            let Some(pointer) =
                point_in(&handle, &backdrop, start_x + offset_x, start_y + offset_y)
            else {
                return;
            };

            let mut current = placement.get();
            let wanted = (
                begin.offset.0 + (pointer.0 - begin.pointer.0).round() as i32,
                begin.offset.1 + (pointer.1 - begin.pointer.1).round() as i32,
            );
            let offset = current.clamp_offset(
                (backdrop.width() - card.width()).max(0),
                (backdrop.height() - card.height()).max(0),
                wanted,
            );
            if offset == current.offset {
                return;
            }
            current.offset = offset;
            placement.set(current);
            backdrop.queue_allocate();
        }
    });

    gesture.connect_drag_end({
        let handle = content.title_handle.clone();
        move |_, _, _| {
            start.set(None);
            handle.set_cursor_from_name(Some("grab"));
        }
    });

    content.title_handle.add_controller(gesture);
}

/// Where a resize drag started, so later updates are deltas from it.
#[derive(Clone, Copy)]
struct ResizeStart {
    /// Pointer in backdrop (output) coordinates.
    pointer: (f64, f64),
    /// Top-left of the card in backdrop coordinates, held for the whole drag.
    origin: (i32, i32),
    /// Card size on screen.
    card: (i32, i32),
    /// Effective list size (the scrolled window's minimum content size).
    list: (i32, i32),
}

/// Drive the list size from drags on the resize grips and persist the result.
///
/// The card's top-left is pinned for the drag. Otherwise the placement would
/// recompute it from the anchor on every layout, and a card pushed against an
/// output edge (or centered) would slide as it changes size instead of letting
/// the dragged border follow the pointer. With the top-left fixed, the right
/// and bottom borders move exactly as far as the pointer does.
///
/// The grips sit on the card, so they move as it grows. The backdrop covers the
/// whole output and never moves, so the pointer is tracked in backdrop
/// coordinates and the size is the start size plus the pointer travel since the
/// drag began. The result is applied through `ListSize` and saved on release.
fn attach_resize_gestures(
    backdrop: &Overlay,
    card: &Overlay,
    content: &OverlayContent,
    placement: &Rc<Cell<CardPlacement>>,
    config_file: &Path,
) {
    for handle in &content.resize_handles {
        let gesture = gtk4::GestureDrag::new();
        let horizontal = handle.horizontal;
        let vertical = handle.vertical;
        let start: Rc<Cell<Option<ResizeStart>>> = Rc::new(Cell::new(None));

        gesture.connect_drag_begin({
            let backdrop = backdrop.clone();
            let card = card.clone();
            let widget = handle.widget.clone();
            let size = content.list_size.clone();
            let placement = placement.clone();
            let start = start.clone();
            move |gesture, start_x, start_y| {
                // Keep the header bar's window handle from taking the sequence.
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                let (Some(pointer), Some(corner)) = (
                    point_in(&widget, &backdrop, start_x, start_y),
                    point_in(&card, &backdrop, 0.0, 0.0),
                ) else {
                    return;
                };
                let origin = (corner.0.round() as i32, corner.1.round() as i32);
                let mut current = placement.get();
                current.pinned = Some(origin);
                placement.set(current);
                start.set(Some(ResizeStart {
                    pointer,
                    origin,
                    card: (card.width(), card.height()),
                    list: (
                        size.scrolled.min_content_width(),
                        size.scrolled.min_content_height(),
                    ),
                }));
            }
        });

        gesture.connect_drag_update({
            let backdrop = backdrop.clone();
            let widget = handle.widget.clone();
            let size = content.list_size.clone();
            let placement = placement.clone();
            let start = start.clone();
            move |gesture, offset_x, offset_y| {
                let (Some(begin), Some((start_x, start_y))) = (start.get(), gesture.start_point())
                else {
                    return;
                };
                // start_point + offset is the pointer in the grip's current
                // coordinate space, which we then map into backdrop space.
                let Some(pointer) =
                    point_in(&widget, &backdrop, start_x + offset_x, start_y + offset_y)
                else {
                    return;
                };

                let margin = placement.get().margin;
                let travel_x = (pointer.0 - begin.pointer.0).round() as i32;
                let travel_y = (pointer.1 - begin.pointer.1).round() as i32;
                let (requested_width, requested_height) = size.requested.get();

                // The top-left is fixed, so the card may grow until its far
                // border reaches the output margin.
                let width = if horizontal {
                    let lower = size.header_minimum().max(OVERLAY_MIN_WIDTH);
                    let room = (backdrop.width() - margin - (begin.origin.0 + begin.card.0)).max(0);
                    let upper = (begin.list.0 + room).max(lower);
                    (begin.list.0 + travel_x).clamp(lower, upper)
                } else {
                    requested_width
                };
                let height = if vertical {
                    let room =
                        (backdrop.height() - margin - (begin.origin.1 + begin.card.1)).max(0);
                    (begin.list.1 + travel_y).clamp(
                        MIN_LIST_HEIGHT.min(begin.list.1),
                        (begin.list.1 + room).max(begin.list.1),
                    )
                } else {
                    requested_height
                };

                if size.requested.get() == (width, height) {
                    return;
                }
                size.set_requested(width, height);
                size.apply(backdrop.width(), backdrop.height(), placement.get());
            }
        });

        gesture.connect_drag_end({
            let size = content.list_size.clone();
            let start = start.clone();
            let config_file = config_file.to_owned();
            move |_, _, _| {
                if start.take().is_none() {
                    return;
                }
                let (width, height) = size.requested.get();
                let saved = UserConfig::read(&config_file)
                    .is_ok_and(|config| config.width == width && config.height == height);
                if saved {
                    return;
                }
                if let Err(e) = UserConfig::update(&config_file, |config| {
                    config.width = width;
                    config.height = height;
                }) {
                    warn!("Failed to save window size: {}", e);
                }
            }
        });

        handle.widget.add_controller(gesture);
    }
}

/// Translate a point from a widget's coordinate space into another widget's.
fn point_in(
    widget: &impl IsA<gtk4::Widget>,
    target: &impl IsA<gtk4::Widget>,
    x: f64,
    y: f64,
) -> Option<(f64, f64)> {
    let point = gtk4::graphene::Point::new(x as f32, y as f32);
    widget
        .compute_point(target, &point)
        .map(|translated| (f64::from(translated.x()), f64::from(translated.y())))
}

/// Build the key controller: Esc hides and clears search or closes, arrows/Enter
/// navigate and activate, Delete or Ctrl+X removes and Ctrl+P pins the selected item.
/// Any other typing goes to the search field, wherever the focus is.
fn generate_key_controller(
    list_box: &gtk4::ListBox,
    history_state: &HistoryListState,
    search_entry: &SearchEntry,
) -> gtk4::EventControllerKey {
    let controller = gtk4::EventControllerKey::new();
    // Run before the focused widget so Esc and type-to-search always work.
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let list_box_for_keys = list_box.clone();
    let history_state_for_keys = history_state.clone();
    let search_entry_for_keys = search_entry.clone();
    controller.connect_key_pressed(move |controller, key, _, state| {
        use gtk4::gdk::{Key, ModifierType};

        // Modal dialogs own keyboard input; don't search, activate or close history.
        if controller
            .widget()
            .and_then(|widget| widget.downcast::<adw::ApplicationWindow>().ok())
            .is_some_and(|window| window.visible_dialog().is_some())
        {
            return gtk4::glib::Propagation::Proceed;
        }

        if key == Key::Escape {
            if search_entry_for_keys.is_visible() {
                search_entry_for_keys.set_text("");
                search_entry_for_keys.set_visible(false);
                select_first_row(&list_box_for_keys);
            } else {
                request_quit();
            }
            return gtk4::glib::Propagation::Stop;
        }

        // Dimension editors own typing, navigation and Enter while focused.
        let focused = controller
            .widget()
            .and_then(|w| w.root())
            .and_then(|root| root.focus());
        if focused.as_ref().is_some_and(|focus| {
            focus.ancestor(gtk4::Entry::static_type()).is_some() || focus.is::<gtk4::Entry>()
        }) {
            return gtk4::glib::Propagation::Proceed;
        }

        // Letters now go to the search field, so vim-style navigation moves
        // to Ctrl+J/Ctrl+K and works from the search field as well.
        if state.contains(ModifierType::CONTROL_MASK) && matches!(key, Key::j | Key::J) {
            select_next_row(&list_box_for_keys, false);
            return gtk4::glib::Propagation::Stop;
        }
        if state.contains(ModifierType::CONTROL_MASK) && matches!(key, Key::k | Key::K) {
            if !select_previous_row(&list_box_for_keys, false) && search_entry_for_keys.is_visible()
            {
                focus_search_entry(&search_entry_for_keys);
            }
            return gtk4::glib::Propagation::Stop;
        }

        // The search field handles its own editing, Up/Down and Enter.
        if has_focus_within(&search_entry_for_keys) {
            return gtk4::glib::Propagation::Proceed;
        }

        // Own the first keypress while the list holds no focus. With nothing
        // selected the initial focus sits on a header button - GTK hands it to
        // the first focusable widget and draws no focus ring for it, so the
        // window looks inert while a button quietly holds focus.
        //
        // Arrow keys would move focus between those buttons instead of walking
        // the list, so they move into the list instead, after which the
        // ordinary navigation takes over. Enter and Space would fire that
        // button; nothing is highlighted to explain why, so they are swallowed -
        // unless GTK considers the focus visible because the user navigated
        // there with the keyboard, which keeps tabbing to a button and pressing
        // Enter working.
        if !has_focus_within(&list_box_for_keys) {
            let focus_visible = controller
                .widget()
                .and_then(|widget| widget.downcast::<gtk4::Window>().ok())
                .is_some_and(|window| window.gets_focus_visible());
            let selected = list_box_for_keys.selected_row().filter(is_row_shown);
            match key {
                Key::Up | Key::KP_Up | Key::Down | Key::KP_Down
                    if selected.is_none() || !focus_visible =>
                {
                    match selected {
                        Some(row) => {
                            row.grab_focus();
                        }
                        None => select_first_row(&list_box_for_keys),
                    };
                    return gtk4::glib::Propagation::Stop;
                }
                Key::Return | Key::KP_Enter | Key::space | Key::KP_Space
                    if selected.is_none() && !focus_visible =>
                {
                    return gtk4::glib::Propagation::Stop;
                }
                _ => {}
            }
        }

        // Row shortcuts apply when a row itself is focused, not one of its
        // inline delete/pin buttons (those keep Enter/Space for activation).
        let focus_on_row = has_focus_within(&list_box_for_keys)
            && focused.is_none_or(|widget| {
                widget.is::<gtk4::ListBoxRow>() || widget.is::<gtk4::ListBox>()
            });
        let has_shortcut_modifier = state.intersects(
            ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK,
        );

        match key {
            Key::Delete | Key::KP_Delete if focus_on_row => {
                if let Some(id) = selected_item_id(&list_box_for_keys) {
                    delete_item(&list_box_for_keys, &history_state_for_keys, id);
                }
                gtk4::glib::Propagation::Stop
            }
            Key::x | Key::X if focus_on_row && state.contains(ModifierType::CONTROL_MASK) => {
                if let Some(id) = selected_item_id(&list_box_for_keys) {
                    delete_item(&list_box_for_keys, &history_state_for_keys, id);
                }
                gtk4::glib::Propagation::Stop
            }
            Key::p | Key::P if focus_on_row && state.contains(ModifierType::CONTROL_MASK) => {
                if let Some(id) = selected_item_id(&list_box_for_keys) {
                    toggle_item_pin(&list_box_for_keys, &history_state_for_keys, id);
                }
                gtk4::glib::Propagation::Stop
            }
            Key::Up | Key::KP_Up if focus_on_row => {
                if !select_previous_row(&list_box_for_keys, false)
                    && search_entry_for_keys.is_visible()
                {
                    focus_search_entry(&search_entry_for_keys);
                }
                gtk4::glib::Propagation::Stop
            }
            Key::Down | Key::KP_Down if focus_on_row => {
                select_next_row(&list_box_for_keys, false);
                gtk4::glib::Propagation::Stop
            }
            Key::Return | Key::KP_Enter if focus_on_row => {
                if let Some(row) = list_box_for_keys.selected_row() {
                    row.emit_by_name::<()>("activate", &[]);
                }
                gtk4::glib::Propagation::Stop
            }
            // Space keeps activating focused buttons and checkboxes.
            Key::space if !focus_on_row => gtk4::glib::Propagation::Proceed,
            _ if !has_shortcut_modifier
                && ((key == Key::BackSpace && search_entry_for_keys.is_visible())
                    || key.to_unicode().is_some_and(|c| !c.is_control())) =>
            {
                focus_search_entry(&search_entry_for_keys);
                if let Some(text) = search_entry_for_keys.delegate() {
                    controller.forward(&text);
                }
                gtk4::glib::Propagation::Stop
            }
            _ => gtk4::glib::Propagation::Proceed,
        }
    });
    controller
}

/// Show and focus the search field without selecting its text, so typing appends.
fn focus_search_entry(search_entry: &SearchEntry) {
    search_entry.set_visible(true);
    match search_entry
        .delegate()
        .and_then(|editable| editable.downcast::<gtk4::Text>().ok())
    {
        Some(text) => {
            text.grab_focus_without_selecting();
        }
        None => {
            search_entry.grab_focus();
        }
    }
}

fn has_focus_within(widget: &impl IsA<gtk4::Widget>) -> bool {
    widget
        .state_flags()
        .contains(gtk4::StateFlags::FOCUS_WITHIN)
}

fn selected_item_id(list_box: &gtk4::ListBox) -> Option<u64> {
    row_item_id(&list_box.selected_row()?)
}

/// The shown row after the item's row, or before it when it is the last one.
fn neighbour_item_id(list_box: &gtk4::ListBox, item_id: u64) -> Option<u64> {
    let index = list_rows(list_box)
        .into_iter()
        .find(|row| row_item_id(row) == Some(item_id))?
        .index();
    shown_row_from(list_box, index, 1)
        .or_else(|| shown_row_from(list_box, index, -1))
        .and_then(|row| row_item_id(&row))
}

/// Delete an item and move the selection to the row that took its place.
fn delete_item(list_box: &gtk4::ListBox, history_state: &HistoryListState, item_id: u64) {
    if !history_state.can_act_on_rows() {
        return;
    }
    let neighbour_id = neighbour_item_id(list_box, item_id);

    if let Err(error) =
        FrontendClient::new().and_then(|mut client| client.delete_item_by_id(item_id))
    {
        error!("Could not delete clipboard item {item_id}: {error}");
        return;
    }
    history_state
        .all_items
        .borrow_mut()
        .retain(|item| item.item_id != item_id);

    rebuild_list(list_box, history_state);
    match neighbour_id {
        Some(id) => select_row_by_item_id(list_box, id),
        None => select_first_row(list_box),
    }
}

fn toggle_item_pin(list_box: &gtk4::ListBox, history_state: &HistoryListState, item_id: u64) {
    if !history_state.can_act_on_rows() {
        return;
    }
    let Some(pinned) = next_pinned_state(history_state, item_id) else {
        return;
    };

    if let Err(error) =
        FrontendClient::new().and_then(|mut client| client.set_pinned(item_id, pinned))
    {
        error!("Could not update pinned state for clipboard item {item_id}: {error}");
        return;
    }

    apply_pinned_state(history_state, item_id, pinned);
    rebuild_list(list_box, history_state);
    select_row_by_item_id(list_box, item_id);
    debug!("Updated pinned state for clipboard item ID {}", item_id);
}

/// Apply custom CSS styling for modern GNOME-style rounded window
fn apply_custom_styling(window: &adw::ApplicationWindow) {
    let css_provider = gtk4::CssProvider::new();
    let display = gtk4::prelude::WidgetExt::display(window);
    let style_manager = adw::StyleManager::for_display(&display);

    load_overlay_css(&css_provider, style_manager.is_dark());

    {
        let css_provider = css_provider.clone();
        style_manager.connect_dark_notify(move |style_manager| {
            load_overlay_css(&css_provider, style_manager.is_dark());
        });
    }

    gtk4::style_context_add_provider_for_display(
        &display,
        &css_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn load_overlay_css(css_provider: &gtk4::CssProvider, is_dark: bool) {
    css_provider.load_from_data(if is_dark {
        "
        window.cursor-clip-backdrop {
            background: transparent;
            border: none;
            box-shadow: none;
        }

        .cursor-clip-card {
            border-radius: 6px;
            background: #222226;
            background: @window_bg_color;
            color: @window_fg_color;
            border: 1px solid alpha(#ffffff, 0.10);
            border: 1px solid alpha(@window_fg_color, 0.10);
            box-shadow: 0 10px 30px alpha(#000000, 0.22);
        }

        headerbar {
            background: transparent;
            box-shadow: none;
        }

        .clipboard-list {
            background: transparent;
        }

        .clipboard-item {
            background: #343437;
            background: @card_bg_color;
            border: 2px solid transparent;
            border-radius: 5px;
            padding: 2px 2px;
            margin: 3px 6px;
            transition: border-color 150ms ease, box-shadow 150ms ease, background 150ms ease;
        }

        .clipboard-item:hover {
            border-color: #3584E4;
            border-color: @accent_bg_color;
            background: shade(#343437, 1.05);
            background: mix(@card_bg_color, @window_fg_color, 0.08);
        }

        .clipboard-item:selected {
            border-color: #3584E4;
            border-color: @accent_bg_color;
            background: alpha(#3584E4, 0.18);
            background: alpha(@accent_bg_color, 0.18);
        }

        /* The entry currently in the clipboard. An inset bar rather than a
           border, so hovering and selecting the row leave it visible. */
        .clipboard-item.active {
            box-shadow: inset 4px 0 0 #3584E4;
            box-shadow: inset 4px 0 0 @accent_bg_color;
        }

        .clipboard-preview {
            opacity: 0.9;
            color: @window_fg_color;
        }

        .clipboard-preview.monospace {
            font-family: monospace;
        }

        .clipboard-time {
            font-size: 0.8em;
            opacity: 0.6;
            color: @window_fg_color;
        }

        .clipboard-delete {
            color: #bfc3c7;
            color: alpha(@window_fg_color, 0.75);
            padding: 2px 4px;
        }

        .clipboard-pin {
            color: #bfc3c7;
            color: alpha(@window_fg_color, 0.75);
            padding: 2px 4px;
        }

        .clipboard-item:hover .clipboard-delete,
        .clipboard-delete:hover {
            color: #ffffff;
            color: @window_fg_color;
        }

        .clipboard-item:hover .clipboard-pin {
            color: #ffffff;
            color: @window_fg_color;
        }

        .clipboard-pin:hover {
            color: #ffffff;
            color: @window_fg_color;
        }

        .clipboard-pin.pinned {
            color: #ffffff;
            color: @accent_color;
        }

        .pin-slash {
            background-color: #ff7b63;
            background-color: @error_color;
            border-radius: 1px;
            transform: rotate(-45deg);
        }

        .manual-close-button {
            min-width: 28px;
            min-height: 28px;
            padding: 0;
            background: transparent;
            box-shadow: none;
        }

        .manual-close-button:hover,
        .manual-close-button:active {
            background: transparent;
            box-shadow: none;
        }

        .manual-close-icon {
            min-width: 28px;
            min-height: 28px;
            border-radius: 999px;
            background: #343437;
            background: @card_bg_color;
        }

        .manual-close-icon image {
            color: #f4f5f6;
            color: @window_fg_color;
        }

        .manual-close-button:hover .manual-close-icon {
            background: shade(#343437, 1.12);
            background: mix(@card_bg_color, @window_fg_color, 0.12);
        }

        .manual-close-button:hover .manual-close-icon image {
            color: #ffffff;
            color: @window_fg_color;
        }

        .manual-close-button:active .manual-close-icon {
            background: shade(#343437, 0.92);
            background: mix(@card_bg_color, @window_fg_color, 0.04);
        }

        .compact-header-action {
            min-width: 28px;
            min-height: 28px;
            padding-left: 0;
            padding-right: 0;
        }

        .settings-row {
            border-radius: 10px;
            padding: 6px 8px;
            transition: background-color 120ms ease;
        }

        .settings-row:hover,
        .settings-row:focus-within {
            background-color: alpha(@popover_fg_color, 0.08);
        }

        .resize-grip:hover {
            background: alpha(@window_fg_color, 0.12);
            border-radius: 0 0 6px 0;
        }

        .menu-revealer {
            background: #2b2b2f;
            background: @popover_bg_color;
            border: 1px solid alpha(#ffffff, 0.10);
            border: 1px solid alpha(@popover_fg_color, 0.10);
            border-radius: 4px;
            padding: 3px 4px;
            color: @popover_fg_color;
        }
        "
    } else {
        "
        window.cursor-clip-backdrop {
            background: transparent;
            border: none;
            box-shadow: none;
        }

        .cursor-clip-card {
            border-radius: 12px;
            background: #f6f7f9;
            background: @window_bg_color;
            color: @window_fg_color;
            border: 1px solid alpha(#000000, 0.10);
            border: 1px solid alpha(@window_fg_color, 0.10);
            box-shadow: 0 10px 30px alpha(#000000, 0.12);
        }

        headerbar {
            background: transparent;
            box-shadow: none;
        }

        .clipboard-list {
            background: transparent;
        }

        .clipboard-item {
            background: #ffffff;
            background: @card_bg_color;
            border: 2px solid transparent;
            border-radius: 10px;
            padding: 4px 4px;
            margin: 6px 12px;
            transition: border-color 150ms ease, box-shadow 150ms ease, background 150ms ease;
        }

        .clipboard-item:hover {
            border-color: #1c71d8;
            border-color: @accent_bg_color;
            background: shade(#ffffff, 0.96);
            background: mix(@card_bg_color, @window_fg_color, 0.04);
        }

        .clipboard-item:selected {
            border-color: #1c71d8;
            border-color: @accent_bg_color;
            background: alpha(#1c71d8, 0.12);
            background: alpha(@accent_bg_color, 0.12);
        }

        /* The entry currently in the clipboard. An inset bar rather than a
           border, so hovering and selecting the row leave it visible. */
        .clipboard-item.active {
            box-shadow: inset 4px 0 0 #1c71d8;
            box-shadow: inset 4px 0 0 @accent_bg_color;
        }

        .clipboard-preview {
            opacity: 0.9;
            color: @window_fg_color;
        }

        .clipboard-preview.monospace {
            font-family: monospace;
        }

        .clipboard-time {
            font-size: 0.8em;
            opacity: 0.6;
            color: @window_fg_color;
        }

        .clipboard-delete {
            color: #5e6268;
            color: alpha(@window_fg_color, 0.7);
            padding: 2px 4px;
        }

        .clipboard-pin {
            color: #5e6268;
            color: alpha(@window_fg_color, 0.7);
            padding: 2px 4px;
        }

        .clipboard-item:hover .clipboard-delete,
        .clipboard-delete:hover {
            color: #1f2328;
            color: @window_fg_color;
        }

        .clipboard-item:hover .clipboard-pin {
            color: #1f2328;
            color: @window_fg_color;
        }

        .clipboard-pin:hover {
            color: #1f2328;
            color: @window_fg_color;
        }

        .clipboard-pin.pinned {
            color: #1f2328;
            color: @accent_color;
        }

        .pin-slash {
            background-color: #c01c28;
            background-color: @error_color;
            border-radius: 1px;
            transform: rotate(-45deg);
        }

        .manual-close-button {
            min-width: 28px;
            min-height: 28px;
            padding: 0;
            background: transparent;
            box-shadow: none;
        }

        .manual-close-button:hover,
        .manual-close-button:active {
            background: transparent;
            box-shadow: none;
        }

        .manual-close-icon {
            min-width: 28px;
            min-height: 28px;
            border-radius: 999px;
            background: #ffffff;
            background: @card_bg_color;
        }

        .manual-close-icon image {
            color: #1f2328;
            color: @window_fg_color;
        }

        .manual-close-button:hover .manual-close-icon {
            background: shade(#ffffff, 0.92);
            background: mix(@card_bg_color, @window_fg_color, 0.08);
        }

        .manual-close-button:hover .manual-close-icon image {
            color: #111318;
            color: @window_fg_color;
        }

        .manual-close-button:active .manual-close-icon {
            background: shade(#ffffff, 0.86);
            background: mix(@card_bg_color, @window_fg_color, 0.12);
        }

        .compact-header-action {
            min-width: 28px;
            min-height: 28px;
            padding-left: 0;
            padding-right: 0;
        }

        .settings-row {
            border-radius: 10px;
            padding: 6px 8px;
            transition: background-color 120ms ease;
        }

        .settings-row:hover,
        .settings-row:focus-within {
            background-color: alpha(@popover_fg_color, 0.08);
        }

        .resize-grip:hover {
            background: alpha(@window_fg_color, 0.12);
            border-radius: 0 0 6px 0;
        }

        .menu-revealer {
            background: #ffffff;
            background: @popover_bg_color;
            border: 1px solid alpha(#000000, 0.10);
            border: 1px solid alpha(@popover_fg_color, 0.10);
            border-radius: 8px;
            padding: 6px 8px;
            box-shadow: 0 2px 8px alpha(#000000, 0.10);
            color: @popover_fg_color;
        }
        "
    });
}

/// Make the icons compiled in by `build.rs` visible to the icon theme, so the
/// pin renders identically no matter which desktop the overlay runs on.
fn register_bundled_icons() {
    if let Err(e) = gtk4::gio::resources_register_include!("cursor-clip.gresource") {
        warn!("Failed to register bundled icons: {e}");
        return;
    }

    let Some(display) = gtk4::gdk::Display::default() else {
        warn!("No display available; bundled icons were not added to the icon theme");
        return;
    };
    let icon_theme = gtk4::IconTheme::for_display(&display);
    icon_theme.add_resource_path(BUNDLED_ICON_RESOURCE_PATH);

    // Without this the only symptom would be a broken-image placeholder on
    // every row, which is easy to miss and hard to trace back.
    for icon in [PIN_ICON, TRASH_ICON] {
        if !icon_theme.has_icon(icon) {
            warn!("Bundled icon {icon} did not resolve after registering its resource path");
        }
    }
}

/// The pin shown on a row: the bare glyph when the item is pinned, and the same
/// glyph behind a red diagonal bar when it is not - the convention most
/// interfaces use for a disabled state. The bar is a styled widget rather than
/// a second icon because no icon theme ships a crossed out pin to pair with.
fn build_pin_icon(pinned: bool) -> gtk4::Widget {
    let image = gtk4::Image::from_icon_name(PIN_ICON);
    image.set_pixel_size(PIN_ICON_PIXEL_SIZE);

    if pinned {
        return image.upcast();
    }

    let slash = Box::new(Orientation::Horizontal, 0);
    slash.add_css_class("pin-slash");
    slash.set_size_request(PIN_SLASH_LENGTH, PIN_SLASH_THICKNESS);
    slash.set_halign(Align::Center);
    slash.set_valign(Align::Center);
    // The bar is decoration; clicks belong to the button underneath it.
    slash.set_can_target(false);

    let overlay = Overlay::new();
    overlay.set_child(Some(&image));
    overlay.add_overlay(&slash);
    overlay.upcast()
}

/// Create a clipboard history item row from backend data
fn generate_listboxrow_from_preview(
    item: &ClipboardItemPreview,
    list_box: &gtk4::ListBox,
    history_state: &HistoryListState,
    show_trash: bool,
    show_pin: bool,
) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    row.add_css_class("clipboard-item");
    row.set_widget_name(&row_key(item));
    if item.active {
        // A marker of its own, because hover and selection both just recolour
        // the border and would otherwise be indistinguishable from it.
        row.add_css_class("active");
    }

    let main_box = Box::new(Orientation::Vertical, 3);
    main_box.set_margin_top(4);
    main_box.set_margin_bottom(4);
    main_box.set_margin_start(6);
    main_box.set_margin_end(6);

    // Header with content type and time
    let header_box = Box::new(Orientation::Horizontal, 4);

    let type_label = Label::new(Some(item.content_type.icon()));
    type_label.add_css_class("caption");

    let type_text = Label::new(Some(item.content_type.as_str()));
    type_text.add_css_class("caption");
    type_text.set_halign(Align::Start);
    type_text.set_hexpand(true);

    let time_label = Label::new(Some(&format_timestamp(item.timestamp)));
    time_label.add_css_class("caption");
    time_label.add_css_class("clipboard-time");
    time_label.set_halign(Align::End);

    let pin_button = Button::new();
    pin_button.set_child(Some(&build_pin_icon(item.pinned)));
    pin_button.add_css_class("flat");
    pin_button.add_css_class("clipboard-pin");
    if item.pinned {
        pin_button.add_css_class("pinned");
        pin_button.set_tooltip_text(Some("Unpin"));
    } else {
        pin_button.set_tooltip_text(Some("Pin"));
    }
    pin_button.set_visible(show_pin);

    let delete_button = Button::builder().icon_name(TRASH_ICON).build();
    delete_button.add_css_class("flat");
    delete_button.add_css_class("destructive-action");
    delete_button.add_css_class("clipboard-delete");
    delete_button.set_tooltip_text(Some("Delete item"));
    delete_button.set_visible(show_trash);

    header_box.append(&type_label);
    header_box.append(&type_text);
    let action_box = Box::new(Orientation::Horizontal, 0);
    action_box.append(&pin_button);
    action_box.append(&delete_button);

    header_box.append(&time_label);
    header_box.append(&action_box);

    main_box.append(&header_box);

    let rendered_image = item.thumbnail.as_ref().and_then(|bytes| {
        let gbytes = gtk4::glib::Bytes::from(bytes);
        gtk4::gdk::Texture::from_bytes(&gbytes).ok()
    });

    if let Some(texture) = rendered_image {
        let picture = gtk4::Picture::for_paintable(&texture);
        picture.set_can_shrink(true);
        picture.set_hexpand(true);
        picture.set_height_request(180);
        picture.set_halign(gtk4::Align::Center);
        picture.add_css_class("clipboard-preview");
        main_box.append(&picture);
    } else {
        // Collapse whitespace runs: Pango cannot wrap inside a long run of spaces, so one
        // indented snippet (e.g. pasted HTML) used to force the whole card to its width.
        let preview: String = item
            .content_preview
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let content_label = Label::new(Some(&preview));
        content_label.add_css_class("clipboard-preview");
        if matches!(
            item.content_type,
            ClipboardContentType::Code | ClipboardContentType::File
        ) {
            content_label.add_css_class("monospace");
        }
        content_label.set_halign(Align::Start);
        content_label.set_wrap(true);
        content_label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
        content_label.set_max_width_chars(100);
        content_label.set_lines(3);
        content_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        main_box.append(&content_label);
    }

    row.set_child(Some(&main_box));

    let item_id = item.item_id;
    delete_button.connect_clicked({
        let list = list_box.downgrade();
        let state = history_state.clone();
        move |_| {
            if let Some(list) = list.upgrade() {
                delete_item(&list, &state, item_id);
            }
        }
    });
    pin_button.connect_clicked({
        let list = list_box.downgrade();
        let state = history_state.clone();
        move |_| {
            if let Some(list) = list.upgrade() {
                toggle_item_pin(&list, &state, item_id);
            }
        }
    });
    row
}

/// Rows built before the first frame; enough to fill the visible part of the list.
const INITIAL_ROWS: usize = 8;
/// Rows added per frame afterwards.
const ROWS_PER_FRAME: usize = 12;

/// Bring the rows in line with `all_items`. Existing rows are reused and only
/// new items get widgets; the first few are built now and the rest follow
/// frame by frame so a cold start does not lay out every row before showing.
fn rebuild_list(list_box: &gtk4::ListBox, history_state: &HistoryListState) {
    let generation = history_state.render_generation.get().wrapping_add(1);
    history_state.render_generation.set(generation);

    // Also remove expired rows while the overlay stays open or resident.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    history_state
        .all_items
        .borrow_mut()
        .retain(|item| item.expires_at.is_none_or(|expires| expires > now));
    if let Some(deadline) = history_state
        .all_items
        .borrow()
        .iter()
        .filter_map(|item| item.expires_at)
        .min()
    {
        let list_box = list_box.downgrade();
        let history_state = history_state.clone();
        gtk4::glib::timeout_add_local_once(
            std::time::Duration::from_secs(deadline.saturating_sub(now)),
            move || {
                if history_state.render_generation.get() == generation
                    && let Some(list_box) = list_box.upgrade()
                {
                    rebuild_list(&list_box, &history_state);
                }
            },
        );
    }

    let done = sync_rows(list_box, history_state, INITIAL_ROWS);
    apply_search_filter(list_box, history_state);
    if done {
        return;
    }

    let history_state = history_state.clone();
    list_box.add_tick_callback(move |list_box, _| {
        if history_state.render_generation.get() != generation {
            return gtk4::glib::ControlFlow::Break;
        }
        let done = sync_rows(list_box, &history_state, ROWS_PER_FRAME);
        // A search match, or the entry in the clipboard, may only exist once
        // its row has been created.
        if list_box.selected_row().filter(is_row_shown).is_none() {
            // Only take focus if the user is not typing a search meanwhile.
            let grab_focus = list_box
                .root()
                .and_then(|root| root.focus())
                .is_none_or(|widget| !widget.is::<gtk4::Text>());
            select_initial_row(list_box, &history_state, grab_focus);
        }
        if done {
            gtk4::glib::ControlFlow::Break
        } else {
            gtk4::glib::ControlFlow::Continue
        }
    });
}

/// One pass of row reconciliation creating at most `budget` new rows.
/// Returns true once the rows match `all_items` exactly.
fn sync_rows(list_box: &gtk4::ListBox, history_state: &HistoryListState, budget: usize) -> bool {
    let items = history_state.all_items.borrow().clone();
    let wanted: std::collections::HashSet<String> = items.iter().map(row_key).collect();

    let mut existing = std::collections::HashMap::new();
    for row in list_rows(list_box) {
        let key = row.widget_name().to_string();
        if wanted.contains(&key) && !existing.contains_key(&key) {
            existing.insert(key, row);
        } else {
            list_box.remove(&row);
        }
    }

    let show_trash = *history_state.show_trash.borrow();
    let show_pin = *history_state.show_pin.borrow();
    let mut created = 0;
    for (position, item) in items.iter().enumerate() {
        let key = row_key(item);
        let index = position as i32;
        if list_box
            .row_at_index(index)
            .is_some_and(|row| row.widget_name() == key)
        {
            continue;
        }
        if let Some(row) = existing.get(&key) {
            list_box.remove(row);
            list_box.insert(row, index);
            continue;
        }
        if created == budget {
            return false;
        }
        let row =
            generate_listboxrow_from_preview(item, list_box, history_state, show_trash, show_pin);
        list_box.insert(&row, index);
        created += 1;
    }
    true
}

fn apply_search_filter(list_box: &gtk4::ListBox, history_state: &HistoryListState) {
    list_box.invalidate_filter();
    history_state
        .placeholder
        .set_label(if history_state.all_items.borrow().is_empty() {
            "No clipboard history yet"
        } else {
            "No matches found"
        });
    select_initial_row(list_box, history_state, false);
}

/// Row widget name: item id, pinned state and a hash of what the row shows.
/// A pin change rebuilds the row, and so does an id that the daemon reused
/// for different content (ids restart when history is not persistent).
fn row_key(item: &ClipboardItemPreview) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    item.timestamp.hash(&mut hasher);
    // The active marker is a style class set when the row is built.
    item.active.hash(&mut hasher);
    item.content_preview.hash(&mut hasher);
    item.thumbnail.as_deref().hash(&mut hasher);
    format!(
        "{}:{}:{:016x}",
        item.item_id,
        u8::from(item.pinned),
        hasher.finish()
    )
}

fn row_item_id(row: &gtk4::ListBoxRow) -> Option<u64> {
    row.widget_name().split(':').next()?.parse().ok()
}

fn list_rows(list_box: &gtk4::ListBox) -> Vec<gtk4::ListBoxRow> {
    let mut rows = Vec::new();
    let mut child = list_box.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Ok(row) = widget.downcast::<gtk4::ListBoxRow>() {
            rows.push(row);
        }
    }
    rows
}

/// Rows hidden by the search filter are skipped by keyboard navigation.
fn is_row_shown(row: &gtk4::ListBoxRow) -> bool {
    row.is_child_visible() && row.is_selectable()
}

/// Relative times ("5 minutes ago") of reused rows go stale while hidden.
fn refresh_time_labels(list_box: &gtk4::ListBox, history_state: &HistoryListState) {
    let items = history_state.all_items.borrow();
    for row in list_rows(list_box) {
        let Some(item) = row_item_id(&row).and_then(|id| items.iter().find(|i| i.item_id == id))
        else {
            continue;
        };
        if let Some(label) = find_descendant_with_class(row.upcast_ref(), "clipboard-time")
            .and_then(|widget| widget.downcast::<Label>().ok())
        {
            label.set_label(&format_timestamp(item.timestamp));
        }
    }
}

fn find_descendant_with_class(widget: &gtk4::Widget, class_name: &str) -> Option<gtk4::Widget> {
    let mut child = widget.first_child();
    while let Some(current) = child {
        if current.has_css_class(class_name) {
            return Some(current);
        }
        if let Some(found) = find_descendant_with_class(&current, class_name) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

fn item_matches_query(item: &ClipboardItemPreview, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }

    item.content_preview.to_lowercase().contains(query)
        || item.content_type.as_str().to_lowercase().contains(query)
}

fn select_first_row(list_box: &gtk4::ListBox) {
    select_first_row_with_focus(list_box, true);
}

fn select_first_row_with_focus(list_box: &gtk4::ListBox, grab_focus: bool) -> bool {
    let Some(row) = list_rows(list_box).into_iter().find(is_row_shown) else {
        list_box.unselect_all();
        return false;
    };
    list_box.select_row(Some(&row));
    if grab_focus {
        row.grab_focus();
    }
    true
}

/// Next shown row after (`step` = 1) or before (`step` = -1) `index`.
fn shown_row_from(list_box: &gtk4::ListBox, index: i32, step: i32) -> Option<gtk4::ListBoxRow> {
    let mut index = index + step;
    while let Some(row) = list_box.row_at_index(index) {
        if is_row_shown(&row) {
            return Some(row);
        }
        index += step;
    }
    None
}

fn select_adjacent_row(list_box: &gtk4::ListBox, step: i32, wrap_to_first: bool) -> bool {
    let Some(current) = list_box.selected_row().filter(is_row_shown) else {
        return select_first_row_with_focus(list_box, true);
    };
    if let Some(row) = shown_row_from(list_box, current.index(), step) {
        list_box.select_row(Some(&row));
        row.grab_focus();
        return true;
    }
    wrap_to_first && select_first_row_with_focus(list_box, true)
}

fn select_next_row(list_box: &gtk4::ListBox, wrap_to_first: bool) -> bool {
    select_adjacent_row(list_box, 1, wrap_to_first)
}

fn select_previous_row(list_box: &gtk4::ListBox, wrap_to_first: bool) -> bool {
    select_adjacent_row(list_box, -1, wrap_to_first)
}

fn select_row_by_item_id(list_box: &gtk4::ListBox, item_id: u64) {
    match list_rows(list_box)
        .into_iter()
        .find(|row| row_item_id(row) == Some(item_id) && is_row_shown(row))
    {
        Some(row) => {
            list_box.select_row(Some(&row));
            row.grab_focus();
        }
        None => {
            select_first_row(list_box);
        }
    }
}

/// The entry currently held in the clipboard, as reported by the backend.
/// `None` when the backend owns no selection, which is the case in
/// monitor-only mode.
fn active_item_id(history_state: &HistoryListState) -> Option<u64> {
    history_state
        .all_items
        .borrow()
        .iter()
        .find(|item| item.active)
        .map(|item| item.item_id)
}

/// Select the row a list starts on. While searching that is the first match.
/// Otherwise it is whichever entry is actually in the clipboard - and there
/// may be none: the daemon owns the selection, so restarting the service
/// leaves the clipboard empty. Nothing is highlighted then, rather than
/// falling back to an arbitrary row.
fn select_initial_row(
    list_box: &gtk4::ListBox,
    history_state: &HistoryListState,
    grab_focus: bool,
) {
    if !history_state.search_query.borrow().trim().is_empty() {
        select_first_row_with_focus(list_box, grab_focus);
        return;
    }
    let active_row = active_item_id(history_state).and_then(|item_id| {
        list_rows(list_box)
            .into_iter()
            .find(|row| row_item_id(row) == Some(item_id) && is_row_shown(row))
    });
    match active_row {
        Some(row) => {
            list_box.select_row(Some(&row));
            if grab_focus {
                row.grab_focus();
            }
        }
        None => list_box.unselect_all(),
    }
}

fn next_pinned_state(history_state: &HistoryListState, item_id: u64) -> Option<bool> {
    history_state
        .all_items
        .borrow()
        .iter()
        .find(|item| item.item_id == item_id)
        .map(|item| !item.pinned)
}

fn apply_pinned_state(history_state: &HistoryListState, item_id: u64, pinned: bool) {
    let mut items = history_state.all_items.borrow_mut();
    let Some(index) = items.iter().position(|entry| entry.item_id == item_id) else {
        return;
    };

    let mut item = items.remove(index);
    item.pinned = pinned;
    let insert_index = if pinned {
        0
    } else {
        items
            .iter()
            .position(|existing| !existing.pinned)
            .unwrap_or(items.len())
    };
    items.insert(insert_index, item);
}

fn set_delete_buttons_visible(list_box: &gtk4::ListBox, visible: bool) {
    let mut child = list_box.first_child();
    while let Some(widget) = child {
        if let Ok(row) = widget.clone().downcast::<gtk4::ListBoxRow>()
            && let Some(delete_button) = find_button_in_row(&row, "clipboard-delete")
        {
            delete_button.set_visible(visible);
        }
        child = widget.next_sibling();
    }
}

fn set_pin_icons_visible(list_box: &gtk4::ListBox, visible: bool) {
    let mut child = list_box.first_child();
    while let Some(widget) = child {
        if let Ok(row) = widget.clone().downcast::<gtk4::ListBoxRow>()
            && let Some(pin_button) = find_button_in_row(&row, "clipboard-pin")
        {
            pin_button.set_visible(visible);
        }
        child = widget.next_sibling();
    }
}
fn find_button_in_row(row: &gtk4::ListBoxRow, class_name: &str) -> Option<gtk4::Button> {
    let main_box = row.child()?.downcast::<gtk4::Box>().ok()?;
    let header_box = main_box.first_child()?.downcast::<gtk4::Box>().ok()?;
    let mut child = header_box.first_child();
    while let Some(widget) = child {
        if widget.has_css_class(class_name) {
            return widget.downcast::<gtk4::Button>().ok();
        }
        if let Ok(container) = widget.clone().downcast::<gtk4::Box>() {
            let mut inner = container.first_child();
            while let Some(inner_widget) = inner {
                if inner_widget.has_css_class(class_name) {
                    return inner_widget.downcast::<gtk4::Button>().ok();
                }
                inner = inner_widget.next_sibling();
            }
        }
        child = widget.next_sibling();
    }
    None
}

/// Format Unix timestamp to relative time string
fn format_timestamp(timestamp: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let diff = now.saturating_sub(timestamp);

    if diff < 30 {
        "Just now".to_string()
    } else if diff < 3600 {
        let minutes = diff / 60;
        format!(
            "{} minute{} ago",
            minutes,
            if minutes == 1 { "" } else { "s" }
        )
    } else if diff < 86400 {
        let hours = diff / 3600;
        format!("{} hour{} ago", hours, if hours == 1 { "" } else { "s" })
    } else {
        let days = diff / 86400;
        format!("{} day{} ago", days, if days == 1 { "" } else { "s" })
    }
}

#[cfg(test)]
mod size_tests {
    use super::*;
    use crate::shared::config::{DEFAULT_LIST_HEIGHT, DEFAULT_LIST_WIDTH, DEFAULT_MARGIN};
    use std::fs;

    #[test]
    #[ignore = "requires a GTK display"]
    fn type_in_toggle_persists_and_follows_instant_paste() {
        fn check_for(widget: &gtk4::Widget, title: &str) -> Option<CheckButton> {
            if let Some(label) = widget.downcast_ref::<Label>()
                && label.text() == title
            {
                return label.mnemonic_widget()?.downcast().ok();
            }
            std::iter::successors(widget.first_child(), |child| child.next_sibling())
                .find_map(|child| check_for(&child, title))
        }
        crate::test_support::isolated_test(|| {
            adw::init().unwrap();
            let path = config_path();
            UserConfig::update(&path, |cfg| cfg.type_instead_of_paste = true).unwrap();
            let content = generate_overlay_content(UserConfig::read(&path).unwrap());
            let menu = content.menu_revealer.upcast_ref();
            let type_in = check_for(menu, "Type in").unwrap();
            let instant = check_for(menu, "Instant paste").unwrap();
            assert!(type_in.is_active());
            instant.set_active(false);
            assert!(!type_in.is_sensitive());
            instant.set_active(true);
            assert!(type_in.is_sensitive());
            for active in [false, true] {
                type_in.emit_activate();
                assert_eq!(
                    UserConfig::read(&path).unwrap().type_instead_of_paste,
                    active
                );
            }
        });
    }

    #[test]
    fn malformed_config_survives_loading_and_preference_changes() {
        let dir =
            std::env::temp_dir().join(format!("cursor-clip-config-{:x}", rand::random::<u64>()));
        let path = dir.join("config.toml");
        assert_eq!(
            UserConfig::load_or_create(&path).unwrap().width,
            DEFAULT_LIST_WIDTH
        );
        for invalid in [
            "width = [",
            "height = 0",
            "height = \"400\"",
            "persistence_enabled = true\nwidth = \"700\"\n",
            "widht = 700\n",
            "position = \"side\"",
            "margin = -1",
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(overlay_config(&path).is_none());
            assert!(UserConfig::update(&path, |cfg| cfg.show_pin = false).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
        }
        fs::write(&path, "width = \"bad\"\npersistent_history = true\n").unwrap();
        assert!(crate::shared::config::read_persistence_enabled(&path).unwrap());
        fs::write(&path, "width = 700\npersistent_history = true\n").unwrap();
        UserConfig::update(&path, |cfg| cfg.show_pin = false).unwrap();
        let config = UserConfig::load_or_create(&path).unwrap();
        assert!(config.persistence_enabled && !config.show_pin);
        assert!(!config.type_instead_of_paste);
        assert_eq!((config.width, config.height), (700, DEFAULT_LIST_HEIGHT));
        assert_eq!(
            (config.position, config.margin),
            (PopupPosition::Cursor, DEFAULT_MARGIN)
        );

        fs::write(
            &path,
            "position = \"bottom-right\"\nmargin = 24\ntype_instead_of_paste = true\n",
        )
        .unwrap();
        UserConfig::update(&path, |cfg| cfg.show_pin = false).unwrap();
        let config = UserConfig::read(&path).unwrap();
        assert!(config.type_instead_of_paste);
        assert_eq!(
            (config.position, config.margin),
            (PopupPosition::BottomRight, 24)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn card_origins_respect_the_output_and_margin() {
        let mut placement = CardPlacement {
            position: PopupPosition::Cursor,
            margin: 24,
            cursor: (125.0, 480.0),
            offset: (0, 0),
            pinned: None,
        };
        for (position, origin) in [
            (PopupPosition::Cursor, (125, 326)),
            (PopupPosition::Center, (250, 175)),
            (PopupPosition::TopLeft, (24, 24)),
            (PopupPosition::TopRight, (476, 24)),
            (PopupPosition::BottomLeft, (24, 326)),
            (PopupPosition::BottomRight, (476, 326)),
        ] {
            placement.position = position;
            assert_eq!(placement.origin(500, 350), origin, "{position:?}");
        }
        assert_eq!(placement.origin(12, 0), (0, 0));
        placement.margin = 0;
        assert_eq!(placement.origin(500, 350), (500, 350));
    }

    #[test]
    fn dragged_cards_stay_inside_the_output_margins() {
        let mut placement = CardPlacement {
            position: PopupPosition::Center,
            margin: 24,
            cursor: (0.0, 0.0),
            offset: (30, -40),
            pinned: None,
        };
        assert_eq!(placement.origin(500, 350), (280, 135));
        // A drag past the edge is limited to the margin.
        placement.offset = placement.clamp_offset(500, 350, (1000, -1000));
        assert_eq!(placement.origin(500, 350), (476, 24));
        // Dragging inward from a pinned corner works from that corner.
        placement.position = PopupPosition::BottomRight;
        placement.offset = (-100, -100);
        assert_eq!(placement.origin(500, 350), (376, 226));
    }

    #[test]
    fn a_pinned_card_keeps_its_top_left_while_it_changes_size() {
        // Opened low on a 1000px output: the anchor pushes a 450px card up so
        // that it ends at the bottom margin.
        let mut placement = CardPlacement {
            position: PopupPosition::Cursor,
            margin: 5,
            cursor: (100.0, 800.0),
            offset: (0, 0),
            pinned: None,
        };
        let card_height = |height: i32| (1000 - height, 1000);
        let (free_height, _) = card_height(450);
        assert_eq!(placement.origin(800, free_height), (100, 545));

        // Unpinned, shrinking the card slides it down: the top edge moves and
        // the bottom edge stays at the output margin, so a dragged bottom
        // border could not follow the pointer.
        let (free_height, _) = card_height(350);
        assert_eq!(placement.origin(800, free_height), (100, 645));

        // Pinned at drag start, the top stays put and the bottom border moves
        // by exactly the size change, whichever way the card goes.
        placement.pinned = Some((100, 545));
        for height in [450, 350, 250, 450] {
            let (free_height, _) = card_height(height);
            let (_, top) = placement.origin(800, free_height);
            assert_eq!((top, top + height), (545, 545 + height), "{height}");
        }

        // A pinned card that no longer fits is pulled back inside the margins.
        let (free_height, _) = card_height(600);
        assert_eq!(placement.origin(800, free_height), (100, 395));
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn oversized_list_fits_the_output_at_the_cursor_and_in_the_center() {
        adw::init().unwrap();
        let config = UserConfig {
            width: 1100,
            height: 900,
            margin: 32,
            ..UserConfig::default()
        };
        let placement = Rc::new(Cell::new(CardPlacement {
            position: config.position,
            margin: config.margin,
            cursor: (798.0, 599.0),
            offset: (0, 0),
            pinned: None,
        }));
        let content = generate_overlay_content(config);
        let window = adw::ApplicationWindow::builder().build();
        apply_custom_styling(&window);
        content.overlay.add_css_class("cursor-clip-card");
        let root = create_backdrop(&content.overlay, placement.clone());
        window.set_content(Some(&root));
        content.list_size.apply(0, 0, placement.get());

        for (position, inset) in [(PopupPosition::Cursor, 32), (PopupPosition::Center, 0)] {
            placement.set(CardPlacement {
                position,
                ..placement.get()
            });
            content.list_size.apply(800, 600, placement.get());
            root.allocate(800, 600, -1, None);
            let bounds = content.overlay.compute_bounds(&root).unwrap();
            assert_eq!((bounds.x(), bounds.y()), (inset as f32, inset as f32));
            assert_eq!(bounds.width(), (800 - 2 * inset) as f32);
            assert_eq!(bounds.height(), (600 - 2 * inset) as f32);
        }

        content.list_size.apply(1920, 1080, placement.get());
        root.allocate(1920, 1080, -1, None);
        assert_eq!(content.list_size.scrolled.width(), 1100);
        assert_eq!(content.list_size.scrolled.height(), 900);
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn list_size_applies_both_dimensions() {
        adw::init().unwrap();
        let config = UserConfig::default();
        let dir =
            std::env::temp_dir().join(format!("cursor-clip-size-{:x}", rand::random::<u64>()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let item = ClipboardItemPreview {
            item_id: 1,
            content_preview: "long".repeat(300),
            content_type: ClipboardContentType::Text,
            timestamp: 1,
            pinned: false,
            active: false,
            thumbnail: None,
            expires_at: None,
        };
        let window = adw::ApplicationWindow::builder()
            .default_width(1000)
            .default_height(550)
            .build();
        apply_custom_styling(&window);
        let content = generate_overlay_content(config);
        apply_history(&content.list_box, &content.history_state, vec![item]);
        content.overlay.add_css_class("cursor-clip-card");
        let placement = Rc::new(Cell::new(CardPlacement {
            position: PopupPosition::BottomRight,
            margin: 24,
            cursor: (990.0, 540.0),
            offset: (0, 0),
            pinned: None,
        }));
        let root = create_backdrop(&content.overlay, placement.clone());
        window.set_content(Some(&root));
        let size = &content.list_size;
        let minimum = size
            .header
            .measure(Orientation::Horizontal, -1)
            .0
            .max(OVERLAY_MIN_WIDTH);
        let wait_for_size = |width, height| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while (size.scrolled.width(), size.scrolled.height()) != (width, height) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "expected {width} × {height}, allocated {} × {}",
                    size.scrolled.width(),
                    size.scrolled.height()
                );
                gtk4::glib::MainContext::default().iteration(false);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        size.apply(1000, 550, placement.get());
        window.present();
        wait_for_size(350, 400);
        let closed_width = content.overlay.width();

        content.persistence.spinner.set_visible(true);
        content
            .persistence
            .status
            .set_label("Updating history storage…");
        content.persistence.status.set_visible(true);
        content
            .persistence
            .history_status
            .set_label("Could not refresh clipboard history. Reopen to retry.");
        content.persistence.history_status.set_visible(true);
        content.menu_revealer.set_visible(true);
        content.menu_revealer.set_reveal_child(true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !content.menu_revealer.is_child_revealed() {
            assert!(std::time::Instant::now() < deadline);
            gtk4::glib::MainContext::default().iteration(false);
        }
        assert_eq!(
            content.overlay.width(),
            closed_width,
            "opening settings widened the card"
        );
        assert_eq!(size.scrolled.width(), 350);
        content.persistence.spinner.set_visible(false);
        content.persistence.status.set_visible(false);
        content.persistence.history_status.set_visible(false);
        size.set_requested(600, 280);
        size.apply(1000, 550, placement.get());
        wait_for_size(600.max(minimum), 280);
        let title = content
            .title_handle
            .first_child()
            .unwrap()
            .downcast::<Label>()
            .unwrap();
        assert!(
            title.width()
                >= title
                    .create_pango_layout(Some(&title.text()))
                    .pixel_size()
                    .0,
            "header title was truncated on a wide card: {} pixels",
            title.width()
        );
        let bounds = content.overlay.compute_bounds(&root).unwrap();
        assert_eq!(bounds.x() + bounds.width(), (root.width() - 24) as f32);
        assert_eq!(bounds.y() + bounds.height(), (root.height() - 24) as f32);

        window.set_visible(false);
        content.menu_revealer.set_visible(false);
        content.search_entry.set_visible(true);
        *content.history_state.search_query.borrow_mut() = "no matches".into();
        apply_search_filter(&content.list_box, &content.history_state);
        UserConfig::update(&path, |cfg| {
            cfg.width = 1;
            cfg.height = 190;
        })
        .unwrap();
        let cfg = UserConfig::load_or_create(&path).unwrap();
        size.set_requested(cfg.width, cfg.height);
        size.apply(1000, 550, placement.get());
        window.present();
        wait_for_size(minimum, 190);
        window.close();
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::{BackendMessage, FrontendMessage};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    #[track_caller]
    fn until<T>(mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(value) = check() {
                return value;
            }
            assert!(Instant::now() < deadline, "GTK condition timed out");
            gtk4::glib::MainContext::default().iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn reply(mut stream: UnixStream, response: BackendMessage) {
        writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
    }

    fn history() -> BackendMessage {
        BackendMessage::History {
            items: vec![ClipboardItemPreview {
                item_id: 1,
                content_type: ClipboardContentType::Text,
                content_preview: "first".into(),
                timestamp: 0,
                pinned: false,
                // The list starts on the entry that is in the clipboard.
                active: true,
                expires_at: None,
                thumbnail: None,
            }],
        }
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn row_actions_use_the_target_id_and_wait_for_acknowledgement() {
        crate::test_support::isolated_test(|| {
            adw::init().unwrap();
            let dir = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
                .join("cursor-clip");
            std::fs::create_dir(&dir).unwrap();
            let listener = UnixListener::bind(dir.join("cursor-clip.sock")).unwrap();
            let server = std::thread::spawn(move || {
                for (expected, response) in [
                    (
                        FrontendMessage::SetPinned {
                            id: 2,
                            pinned: true,
                        },
                        BackendMessage::ItemPinned {
                            id: 2,
                            pinned: true,
                        },
                    ),
                    (
                        FrontendMessage::SetPinned {
                            id: 2,
                            pinned: false,
                        },
                        BackendMessage::ItemPinned {
                            id: 2,
                            pinned: false,
                        },
                    ),
                    (
                        FrontendMessage::DeleteItemById { id: 2 },
                        BackendMessage::Error {
                            message: "rejected".into(),
                        },
                    ),
                    (
                        FrontendMessage::DeleteItemById { id: 2 },
                        BackendMessage::ItemDeleted { id: 1 },
                    ),
                    (
                        FrontendMessage::DeleteItemById { id: 2 },
                        BackendMessage::ItemDeleted { id: 2 },
                    ),
                ] {
                    let (stream, _) = listener.accept().unwrap();
                    let mut line = String::new();
                    BufReader::new(&stream).read_line(&mut line).unwrap();
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(&line).unwrap(),
                        serde_json::to_value(expected).unwrap()
                    );
                    reply(stream, response);
                }
            });

            let content = generate_overlay_content(UserConfig::default());
            let list = &content.list_box;
            let state = &content.history_state;
            let window = adw::ApplicationWindow::builder().build();
            window.set_content(Some(&content.overlay));
            let keys = generate_key_controller(list, state, &content.search_entry);
            window.add_controller(keys.clone());
            window.present();
            let BackendMessage::History { mut items } = history() else {
                unreachable!()
            };
            items.push(ClipboardItemPreview {
                item_id: 2,
                ..items[0].clone()
            });
            apply_history(list, state, items);
            let click = |id, class| {
                let row = list_rows(list)
                    .into_iter()
                    .find(|row| row_item_id(row) == Some(id))
                    .unwrap();
                find_button_in_row(&row, class).unwrap().emit_clicked();
            };
            let press = |key: gtk4::gdk::Key, modifiers: gtk4::gdk::ModifierType| {
                assert!(keys.emit_by_name::<bool>("key-pressed", &[&key, &0u32, &modifiers]));
            };

            // Cached rows are visible but cannot act before the first current snapshot.
            click(2, "clipboard-delete");
            state.current_snapshot.set(true);
            select_first_row(list);
            assert_eq!(selected_item_id(list), Some(1));
            click(2, "clipboard-pin");
            assert!(state.all_items.borrow()[0].pinned);
            assert_eq!(selected_item_id(list), Some(2));
            press(gtk4::gdk::Key::p, gtk4::gdk::ModifierType::CONTROL_MASK);
            assert!(!state.all_items.borrow()[0].pinned);
            press(gtk4::gdk::Key::Delete, gtk4::gdk::ModifierType::empty());
            assert_eq!(state.all_items.borrow().len(), 2);
            select_row_by_item_id(list, 1);
            click(2, "clipboard-delete");
            assert_eq!(state.all_items.borrow().len(), 2);
            click(2, "clipboard-delete");
            assert_eq!(state.all_items.borrow().len(), 1);
            assert_eq!(selected_item_id(list), Some(1));
            window.close();
            server.join().unwrap();
        });
    }

    #[test]
    #[ignore = "requires a GTK display"]
    fn cold_focus_and_persistence_ack_are_independent_of_history() {
        crate::test_support::isolated_test(|| {
            adw::init().unwrap();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let _enter = runtime.enter();
            let dir = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
                .join("cursor-clip");
            std::fs::create_dir(&dir).unwrap();
            let listener = UnixListener::bind(dir.join("cursor-clip.sock")).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let stream = stream.unwrap();
                    let mut line = String::new();
                    BufReader::new(&stream).read_line(&mut line).unwrap();
                    if tx
                        .send((
                            serde_json::from_str::<FrontendMessage>(&line).unwrap(),
                            stream,
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let next_request = || until(|| rx.try_recv().ok());
            let content = generate_overlay_content(UserConfig::default());
            let controls = &content.persistence;
            let window = adw::ApplicationWindow::builder()
                .title("Cursor Clip focus test")
                .build();
            window.set_content(Some(&content.overlay));
            window.connect_map({
                let controls = controls.clone();
                move |_| open_history(&controls)
            });
            window.present();
            let mut initial_history = None;
            let mut query = None;
            for _ in 0..2 {
                match next_request() {
                    (FrontendMessage::GetHistory, stream) => initial_history = Some(stream),
                    (FrontendMessage::GetHistoryPersistence, stream) => query = Some(stream),
                    (message, _) => panic!("Unexpected startup request {message:?}"),
                }
            }
            // GTK dispatched both futures while neither backend reply was available.
            assert!(controls.list_box.is_sensitive());
            reply(initial_history.unwrap(), history());
            until(|| controls.list_box.selected_row());
            assert!(
                controls.list_box.root().and_then(|root| root.focus())
                    == controls.list_box.selected_row().map(|row| row.upcast()),
                "First row must receive keyboard focus on cold open"
            );
            reply(
                query.unwrap(),
                BackendMessage::PersistenceState { enabled: false },
            );
            until(|| controls.check.is_sensitive().then_some(()));

            window.set_visible(false);
            window.present();
            let mut reopened = None;
            for _ in 0..2 {
                match next_request() {
                    (FrontendMessage::GetHistory, stream) => reopened = Some(stream),
                    (FrontendMessage::GetHistoryPersistence, stream) => {
                        reply(stream, BackendMessage::PersistenceState { enabled: false })
                    }
                    (message, _) => panic!("Unexpected reopen request {message:?}"),
                }
            }
            assert!(!controls.history_state.can_act_on_rows());
            let BackendMessage::History { mut items } = history() else {
                unreachable!()
            };
            // The newest copy is now the entry in the clipboard.
            items[0].active = false;
            items.insert(
                0,
                ClipboardItemPreview {
                    item_id: 2,
                    content_preview: "newest".into(),
                    active: true,
                    ..items[0].clone()
                },
            );
            reply(reopened.unwrap(), BackendMessage::History { items });
            until(|| {
                (controls.history_state.can_act_on_rows() && controls.check.is_sensitive())
                    .then_some(())
            });
            assert_eq!(selected_item_id(&controls.list_box), Some(2));
            assert!(
                controls.list_box.root().and_then(|root| root.focus())
                    == controls.list_box.selected_row().map(|row| row.upcast())
            );

            controls.check.set_active(true);
            let (request, transition) = next_request();
            assert!(matches!(
                request,
                FrontendMessage::SetHistoryPersistence { enabled: true }
            ));
            assert!(persistence_pending());
            assert!(!controls.list_box.is_sensitive());
            reply(
                transition,
                BackendMessage::PersistenceState { enabled: true },
            );
            let (request, refresh) = next_request();
            assert!(matches!(request, FrontendMessage::GetHistory));
            assert!(!persistence_pending());
            assert!(controls.check.is_active() && controls.check.is_sensitive());
            assert!(controls.list_box.is_sensitive() && controls.clear_button.is_sensitive());
            focus_search_entry(&content.search_entry);
            reply(refresh, history());
            let generation = controls.history_state.render_generation.get();
            until(|| (controls.history_state.render_generation.get() != generation).then_some(()));
            assert!(
                content
                    .search_entry
                    .root()
                    .and_then(|root| root.focus())
                    .is_some_and(|focus| focus.is_ancestor(&content.search_entry))
            );

            controls.check.set_active(false);
            let (_, rejected) = next_request();
            reply(
                rejected,
                BackendMessage::Error {
                    message: "cannot save setting".into(),
                },
            );
            until(|| (!persistence_pending()).then_some(()));
            assert!(controls.check.is_active() && !controls.check.is_inconsistent());
            let (_, refresh) = next_request();
            drop(refresh);
            until(|| controls.history_status.get_visible().then_some(()));

            controls.check.set_active(false);
            let (_, unconfirmed) = next_request();
            drop(unconfirmed);
            until(|| (!persistence_pending()).then_some(()));
            assert!(controls.check.is_inconsistent());
            let (_, stalled) = next_request();
            OVERLAY_WINDOW.with(|slot| *slot.borrow_mut() = Some(window.clone()));
            request_quit();
            assert!(
                !window.is_visible(),
                "A stalled history refresh held the window open"
            );
            OVERLAY_WINDOW.with(|slot| slot.borrow_mut().take());
            drop(stalled);
            reset_close_flags();
        });
    }
}
