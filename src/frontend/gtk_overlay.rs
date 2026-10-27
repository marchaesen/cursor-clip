use crate::frontend::ipc_client::FrontendClient;
use crate::shared::{ClipboardContentType, ClipboardItemPreview};
use gtk4::prelude::*;
use gtk4::{
    Align, Application, Box, Button, CheckButton, Label, Orientation, Overlay, Revealer,
    SearchEntry,
};
use gtk4_layer_shell::{Edge, Layer, LayerShell};
use libadwaita::{self as adw, prelude::*};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::fs;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::frontend::hyprland;
use crate::frontend::toggle::ToggleServer;
use std::os::fd::AsRawFd;

static INIT: Once = Once::new();
pub static CLOSE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Fallback overlay size used before the user has resized the window once.
const DEFAULT_OVERLAY_WIDTH: i32 = 200;
const DEFAULT_OVERLAY_HEIGHT: i32 = 400;
/// Smallest size the header bar and a readable list row still fit in.
const MIN_OVERLAY_WIDTH: i32 = 200;
const MIN_OVERLAY_HEIGHT: i32 = 160;
/// The card is undecorated, so resizing runs through our own grips.
const RESIZE_HANDLE_THICKNESS: i32 = 6;
const RESIZE_CORNER_SIZE: i32 = 14;
/// Keep the card clear of the monitor edge, matching the spawn clamp.
const SCREEN_EDGE_MARGIN: i32 = 2;

/// Resource path the bundled icons are registered under, and the pin they hold.
/// Shipping the pin ourselves keeps the pinned state looking identical on every
/// desktop rather than depending on whichever icon theme is installed.
const BUNDLED_ICON_RESOURCE_PATH: &str = "/io/github/sirulex/cursor-clip/icons";
const PIN_ICON: &str = "cursor-clip-pin-symbolic";
const PIN_ICON_PIXEL_SIZE: i32 = 16;
/// A red bar drawn diagonally across the pin marks an unpinned row.
const PIN_SLASH_LENGTH: i32 = 16;
const PIN_SLASH_THICKNESS: i32 = 2;
/// Approximate header bar height, used to keep its buttons out of the resize strip.
const HEADER_BAR_HEIGHT: i32 = 42;

#[derive(Clone)]
pub struct OverlayGeometry {
    pub x: f64,
    pub y: f64,
    pub monitor_width: i32,
    pub monitor_height: i32,
    /// Output connector to open on; `None` lets the compositor choose
    pub monitor: Option<String>,
}

// Thread-local storage for the overlay state since GTK objects aren't Send/Sync
thread_local! {
    static OVERLAY_WINDOW: RefCell<Option<adw::ApplicationWindow>> = const { RefCell::new(None) };
    static OVERLAY_APP: RefCell<Option<Application>> = const { RefCell::new(None) };
    // Set when the overlay stays alive between uses (hidden instead of quit).
    static RESIDENT: RefCell<Option<ResidentOverlay>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct UserConfig {
    show_trash: bool,
    show_pin: bool,
    #[serde(alias = "persistent_history")]
    persistence_enabled: bool,
    instant_paste: bool,
    window_width: i32,
    window_height: i32,
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
    placeholder: Label,
}

struct OverlayContent {
    overlay: Overlay,
    title_handle: Box,
    list_box: gtk4::ListBox,
    history_state: HistoryListState,
    search_entry: SearchEntry,
    menu_revealer: Revealer,
    config: Rc<RefCell<UserConfig>>,
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
    card: Overlay,
    list_box: gtk4::ListBox,
    history_state: HistoryListState,
    search_entry: SearchEntry,
    menu_revealer: Revealer,
    /// Pointer position the card is anchored to, relative to the output
    card_target: Rc<Cell<(f64, f64)>>,
    /// Card size the user asked for by dragging a grip (or restored from config)
    card_size: Rc<Cell<(i32, i32)>>,
}

/// A hidden overlay kept in memory with its rows already laid out, so the
/// next open only has to apply history changes and map the window.
struct ResidentOverlay {
    ui: OverlayUi,
    _hold: gtk4::gio::ApplicationHoldGuard,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            show_trash: true,
            show_pin: true,
            persistence_enabled: false,
            instant_paste: true,
            window_width: DEFAULT_OVERLAY_WIDTH,
            window_height: DEFAULT_OVERLAY_HEIGHT,
        }
    }
}

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("cursor-clip")
        .join("config.toml")
}

fn load_or_create_config() -> UserConfig {
    let path = config_path();
    if let Some(parent) = path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        warn!("Failed to create config directory: {}", e);
    }

    if let Ok(contents) = fs::read_to_string(&path)
        && let Ok(config) = toml::from_str::<UserConfig>(&contents)
    {
        return config;
    }

    let config = UserConfig::default();
    if let Err(e) = save_config(&config) {
        warn!("Failed to write default config: {}", e);
    }
    config
}

fn save_config(config: &UserConfig) -> Result<(), std::io::Error> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::write(path, contents)
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
pub fn init_clipboard_overlay(
    geometry: OverlayGeometry,
    prefetched_items: Vec<ClipboardItemPreview>,
    toggle_listener: UnixListener,
    resident: bool,
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

        let ui = create_layer_shell_window(&app_clone, geometry.clone(), prefetched_items.clone());
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

        debug!(
            "Libadwaita overlay window created at ({}, {})",
            geometry.x, geometry.y
        );
    });

    // Keep listening while GTK owns the main loop. A new cursor-clip process
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
    geometry: OverlayGeometry,
    prefetched_items: Vec<ClipboardItemPreview>,
) -> OverlayUi {
    let OverlayGeometry {
        x,
        y,
        monitor_width,
        monitor_height,
        monitor,
    } = geometry;

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

    // Cover the whole output with a transparent surface. The card is placed
    // inside it at the pointer, and a click on the transparent area closes it.
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    if let Some(gdk_monitor) = monitor.as_deref().and_then(find_gdk_monitor) {
        window.set_monitor(Some(&gdk_monitor));
    }

    window.set_exclusive_zone(-1);

    // Make window keyboard interactive
    window.set_keyboard_mode(gtk4_layer_shell::KeyboardMode::Exclusive);

    // Apply custom styling
    apply_custom_styling(&window);

    // Create and set content (also obtain list_box for navigation)
    let content = generate_overlay_content(prefetched_items);
    let card = content.overlay.clone();
    card.add_css_class("cursor-clip-card");
    card.set_overflow(gtk4::Overflow::Hidden);
    card.set_halign(Align::Start);
    card.set_valign(Align::Start);

    let root = Box::new(Orientation::Vertical, 0);
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.append(&card);
    window.set_content(Some(&root));

    // Restore the size the user last dragged the card to. The card is only
    // given a minimum size; the content can still make it larger.
    let restored_size = {
        let config = content.config.borrow();
        clamp_card_size(
            config.window_width,
            config.window_height,
            monitor_width,
            monitor_height,
            (SCREEN_EDGE_MARGIN, SCREEN_EDGE_MARGIN),
        )
    };
    card.set_size_request(restored_size.0, restored_size.1);
    let card_size = Rc::new(Cell::new(restored_size));

    // Position the card before the first frame using its natural size...
    let (_, natural_width, _, _) = card.measure(Orientation::Horizontal, -1);
    let (_, natural_height, _, _) = card.measure(Orientation::Vertical, natural_width);
    place_card(
        &card,
        x,
        y,
        natural_width.max(restored_size.0),
        natural_height.max(restored_size.1),
        monitor_width,
        monitor_height,
    );

    // ...then clamp again with the real allocation, which also covers the
    // case where the output size was unknown up front. The layer surface size
    // is only known once the compositor has configured it, which can be
    // several frames after map, so keep looking until there is a real
    // allocation before shifting the card fully on screen.
    let card_target = Rc::new(Cell::new((x, y)));
    window.connect_map({
        let card = card.clone();
        let card_target = card_target.clone();
        let card_size = card_size.clone();
        move |mapped_window| {
            let card = card.clone();
            let card_target = card_target.clone();
            let card_size = card_size.clone();
            mapped_window.add_tick_callback(move |window, _| {
                if window.width() <= 0 || window.height() <= 0 || card.width() <= 0 {
                    return gtk4::glib::ControlFlow::Continue;
                }
                let (x, y) = card_target.get();
                let (width, height) = card_size.get();
                place_card(
                    &card,
                    x,
                    y,
                    card.width().max(width),
                    card.height().max(height),
                    window.width(),
                    window.height(),
                );
                gtk4::glib::ControlFlow::Break
            });
        }
    });

    attach_resize_gestures(&window, &card, &content, &card_size);
    attach_move_gesture(&window, &card, &content, &card_size, &card_target);

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

    // Start in the history list on the entry that is actually in the
    // clipboard; typing reveals and focuses search on demand.
    //
    // Focus goes on the row, not on the list: GtkListBox answers the arrow keys
    // itself, from the row holding focus, and a list focused on itself has no
    // such row and so ignores them. With no entry in the clipboard nothing is
    // selected or focused - focusing the list then would hand it a cursor row
    // and put a highlight back on the first entry.
    window.connect_map({
        let list_box = content.list_box.clone();
        let history_state = content.history_state.clone();
        move |_| select_initial_row(&list_box, &history_state, true)
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
        card,
        list_box: content.list_box,
        history_state: content.history_state,
        search_entry: content.search_entry,
        menu_revealer: content.menu_revealer,
        card_target,
        card_size,
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
/// reset search/menu/scroll, move the card to the pointer and map it.
fn show_resident_overlay() {
    RESIDENT.with(|resident| {
        let resident = resident.borrow();
        let Some(resident) = resident.as_ref() else {
            return;
        };
        let ui = &resident.ui;

        match FrontendClient::new().and_then(|mut client| client.get_history()) {
            Ok(items) => *ui.history_state.all_items.borrow_mut() = items,
            Err(e) => warn!("Failed to refresh clipboard history: {e}"),
        }

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

        if let Some(location) = hyprland::pointer_location() {
            if let Some(gdk_monitor) = find_gdk_monitor(&location.monitor) {
                ui.window.set_monitor(Some(&gdk_monitor));
            }
            ui.card_target.set((location.x, location.y));
            let (width, height) = ui.card_size.get();
            place_card(
                &ui.card,
                location.x,
                location.y,
                ui.card.width().max(width),
                ui.card.height().max(height),
                location.monitor_width,
                location.monitor_height,
            );
        }

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

/// Keep the card at the pointer but fully on screen.
fn place_card(
    card: &Overlay,
    x: f64,
    y: f64,
    card_width: i32,
    card_height: i32,
    area_width: i32,
    area_height: i32,
) {
    let (left, top) = if area_width > 0 && area_height > 0 {
        let margin = f64::from(SCREEN_EDGE_MARGIN);
        let max_x = (area_width as f64 - card_width as f64 - margin).max(margin);
        let max_y = (area_height as f64 - card_height as f64 - margin).max(margin);
        (x.clamp(margin, max_x), y.clamp(margin, max_y))
    } else {
        (x, y)
    };
    card.set_margin_start(left as i32);
    card.set_margin_top(top as i32);
}

/// Create a Windows 11-style clipboard history list with provided (prefetched) backend data.
/// Falls back to a lazy on-demand fetch only if the provided vector is empty.
fn generate_overlay_content(mut prefetched_items: Vec<ClipboardItemPreview>) -> OverlayContent {
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

    let config_state = Rc::new(RefCell::new(load_or_create_config()));
    let show_trash_default = config_state.borrow().show_trash;
    let show_pin_default = config_state.borrow().show_pin;
    let persistence_enabled_default = config_state.borrow().persistence_enabled;
    let instant_paste_default = config_state.borrow().instant_paste;
    let show_trash_state = Rc::new(RefCell::new(show_trash_default));
    let show_pin_state = Rc::new(RefCell::new(show_pin_default));

    if let Ok(mut client) = FrontendClient::new()
        && let Err(e) = client.set_persistence_enabled(persistence_enabled_default)
    {
        warn!("Failed to sync persistence setting with backend: {}", e);
    }

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
    let toggle_label = Label::new(Some("Show delete button"));
    toggle_label.set_halign(Align::Start);
    toggle_label.set_hexpand(true);
    let toggle_check = CheckButton::new();
    toggle_check.set_active(show_trash_default);
    toggle_row.append(&toggle_label);
    toggle_row.append(&toggle_check);
    menu_box.append(&toggle_row);

    let pin_toggle_row = Box::new(Orientation::Horizontal, 4);
    let pin_toggle_label = Label::new(Some("Show pin icon"));
    pin_toggle_label.set_halign(Align::Start);
    pin_toggle_label.set_hexpand(true);
    let pin_toggle_check = CheckButton::new();
    pin_toggle_check.set_active(show_pin_default);
    pin_toggle_row.append(&pin_toggle_label);
    pin_toggle_row.append(&pin_toggle_check);
    menu_box.append(&pin_toggle_row);

    let persistence_toggle_row = Box::new(Orientation::Horizontal, 4);
    let persistence_toggle_label = Label::new(Some("Persistent history"));
    persistence_toggle_label.set_halign(Align::Start);
    persistence_toggle_label.set_hexpand(true);
    let persistence_toggle_check = CheckButton::new();
    persistence_toggle_check.set_active(persistence_enabled_default);
    persistence_toggle_row.append(&persistence_toggle_label);
    persistence_toggle_row.append(&persistence_toggle_check);
    menu_box.append(&persistence_toggle_row);

    let instant_paste_toggle_row = Box::new(Orientation::Horizontal, 4);
    let instant_paste_toggle_label = Label::new(Some("Instant paste"));
    instant_paste_toggle_label.set_halign(Align::Start);
    instant_paste_toggle_label.set_hexpand(true);
    let instant_paste_toggle_check = CheckButton::new();
    instant_paste_toggle_check.set_active(instant_paste_default);
    instant_paste_toggle_row.append(&instant_paste_toggle_label);
    instant_paste_toggle_row.append(&instant_paste_toggle_check);
    menu_box.append(&instant_paste_toggle_row);

    menu_revealer.set_child(Some(&menu_box));
    header_bar.pack_end(&close_button);
    header_bar.pack_end(&header_action_group);

    close_button.connect_clicked(move |_| {
        request_quit();
    });

    let clear_button = Button::builder().icon_name("user-trash-symbolic").build();
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
    scrolled_window.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scrolled_window.set_min_content_width(MIN_OVERLAY_WIDTH);
    scrolled_window.set_min_content_height(MIN_OVERLAY_HEIGHT);
    // Absorb whatever height the window has beyond the header bar.
    scrolled_window.set_vexpand(true);

    // Create list box for clipboard items
    let list_box = gtk4::ListBox::new();
    // Use custom styling instead of the default boxed-list to create floating cards
    list_box.add_css_class("clipboard-list");
    //list_box.set_margin_top(3);
    list_box.set_margin_bottom(3);
    list_box.set_margin_start(2);
    list_box.set_margin_end(2);
    list_box.set_selection_mode(gtk4::SelectionMode::Single);

    // Start with prefetched items; if empty try one lazy fetch (non-fatal if it fails)

    if prefetched_items.is_empty() {
        debug!("Prefetched clipboard history empty - trying on-demand fetch...");
        if let Ok(mut client) = FrontendClient::new() {
            match client.get_history() {
                Ok(fetched) => prefetched_items = fetched,
                Err(e) => warn!("Error fetching clipboard history on-demand: {}", e),
            }
        }
    }

    let placeholder = Label::new(None);
    placeholder.add_css_class("dim-label");
    placeholder.set_margin_top(10);
    placeholder.set_margin_bottom(10);
    list_box.set_placeholder(Some(&placeholder));

    let history_state = HistoryListState {
        all_items: Rc::new(RefCell::new(prefetched_items)),
        search_query: Rc::new(RefCell::new(String::new())),
        show_trash: show_trash_state,
        show_pin: show_pin_state,
        render_generation: Rc::new(Cell::new(0)),
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
    let config_for_activation = config_state.clone();
    list_box.connect_row_activated(move |_, row| {
        let Some(item_id) = row_item_id(row) else {
            return;
        };
        let items = history_state_for_activation.all_items.borrow();
        if let Some(item) = items.iter().find(|item| item.item_id == item_id) {
            let instant_paste = config_for_activation.borrow().instant_paste;
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
    let config_for_toggle = config_state.clone();
    let history_state_for_toggle = history_state.clone();
    toggle_check.connect_toggled(move |check| {
        let state = check.is_active();
        {
            let mut config = config_for_toggle.borrow_mut();
            config.show_trash = state;
            if let Err(e) = save_config(&config) {
                warn!("Failed to save config: {}", e);
            }
        }
        *history_state_for_toggle.show_trash.borrow_mut() = state;
        set_delete_buttons_visible(&list_box_for_toggle, state);
    });

    let list_box_for_pin_toggle = list_box.clone();
    let config_for_pin_toggle = config_state.clone();
    let history_state_for_pin_toggle = history_state.clone();
    pin_toggle_check.connect_toggled(move |check| {
        let state = check.is_active();
        {
            let mut config = config_for_pin_toggle.borrow_mut();
            config.show_pin = state;
            if let Err(e) = save_config(&config) {
                warn!("Failed to save config: {}", e);
            }
        }
        *history_state_for_pin_toggle.show_pin.borrow_mut() = state;
        set_pin_icons_visible(&list_box_for_pin_toggle, state);
    });

    let config_for_persistence_toggle = config_state.clone();
    persistence_toggle_check.connect_toggled(move |check| {
        let state = check.is_active();
        {
            let mut config = config_for_persistence_toggle.borrow_mut();
            config.persistence_enabled = state;
            if let Err(e) = save_config(&config) {
                warn!("Failed to save config: {}", e);
            }
        }

        match FrontendClient::new() {
            Ok(mut client) => {
                if let Err(e) = client.set_persistence_enabled(state) {
                    warn!("Failed to update persistence in backend: {}", e);
                }
            }
            Err(e) => {
                warn!("Failed to connect to backend for persistence toggle: {}", e);
            }
        }
    });

    let config_for_instant_paste_toggle = config_state.clone();
    instant_paste_toggle_check.connect_toggled(move |check| {
        let state = check.is_active();
        let mut config = config_for_instant_paste_toggle.borrow_mut();
        config.instant_paste = state;
        if let Err(e) = save_config(&config) {
            warn!("Failed to save config: {}", e);
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

    let list_box_for_clear = list_box.clone();
    let history_state_for_clear = history_state.clone();
    clear_button.connect_clicked(move |button| {
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
        let list_box_for_clear = list_box_for_clear.clone();
        let history_state_for_clear = history_state_for_clear.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "clear" {
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

            // Pinned items survive a clear, so ask the backend what is left
            // rather than assuming the history is now empty. The overlay stays
            // open and is refreshed in place.
            match client.get_history() {
                Ok(items) => *history_state_for_clear.all_items.borrow_mut() = items,
                Err(e) => {
                    warn!("Failed to reload clipboard history after clearing: {}", e);
                    history_state_for_clear
                        .all_items
                        .borrow_mut()
                        .retain(|item| item.pinned);
                }
            }

            info!("Clipboard history cleared");
            rebuild_list(&list_box_for_clear, &history_state_for_clear);
            select_first_row(&list_box_for_clear);
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
    overlay.add_overlay(&resize_right);
    overlay.add_overlay(&resize_bottom);
    overlay.add_overlay(&resize_corner);

    OverlayContent {
        overlay,
        title_handle,
        list_box,
        history_state,
        search_entry,
        menu_revealer,
        config: config_state,
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

/// Clamp a requested card size to the space between `origin` (the card's top
/// left corner) and the edge of an `area_width` x `area_height` output. An
/// unknown output size (0) leaves that axis unbounded. Used with a fixed
/// origin when restoring, because a card that does not fit below the cursor is
/// moved up rather than shrunk, and with the live origin while dragging a
/// grip, so a resize stops at the screen edge instead of running off it.
fn clamp_card_size(
    width: i32,
    height: i32,
    area_width: i32,
    area_height: i32,
    origin: (i32, i32),
) -> (i32, i32) {
    let max_width = if area_width > 0 {
        (area_width - origin.0 - SCREEN_EDGE_MARGIN).max(MIN_OVERLAY_WIDTH)
    } else {
        i32::MAX
    };
    let max_height = if area_height > 0 {
        (area_height - origin.1 - SCREEN_EDGE_MARGIN).max(MIN_OVERLAY_HEIGHT)
    } else {
        i32::MAX
    };

    (
        width.clamp(MIN_OVERLAY_WIDTH, max_width),
        height.clamp(MIN_OVERLAY_HEIGHT, max_height),
    )
}

/// Create a transparent drag strip used as a resize grip.
fn build_resize_handle(cursor: &str, halign: Align, valign: Align) -> Box {
    let handle = Box::new(Orientation::Horizontal, 0);
    handle.set_halign(halign);
    handle.set_valign(valign);
    handle.set_cursor_from_name(Some(cursor));
    handle
}

/// Move the card by dragging the header bar's title area.
///
/// The overlay surface covers the whole output and the card sits inside it at
/// its margins, so the header bar's built-in drag support - which asks the
/// compositor to move a toplevel - does nothing here. The position has to be
/// driven from the card margins by hand.
///
/// Two details keep the card following the cursor:
///
/// The gesture claims its sequence up front. `adw::HeaderBar` wraps itself in a
/// window handle carrying a drag gesture of its own, and that gesture takes the
/// sequence over as soon as the 8px drag threshold is crossed - precisely where
/// the movement otherwise stalls after a few pixels.
///
/// The offset is added to the card's *live* origin. Gesture coordinates are
/// relative to the title area, which moves along with the card, so the offset
/// already has the movement applied so far subtracted from it:
/// `offset == travelled - applied`. Adding it to the live origin therefore
/// lands on `start + travelled` and self-corrects, whereas anchoring to the
/// origin captured at drag start subtracts the applied movement a second time
/// and pulls the card back towards where it began.
fn attach_move_gesture(
    window: &adw::ApplicationWindow,
    card: &Overlay,
    content: &OverlayContent,
    card_size: &Rc<Cell<(i32, i32)>>,
    card_target: &Rc<Cell<(f64, f64)>>,
) {
    let gesture = gtk4::GestureDrag::new();

    let handle_for_begin = content.title_handle.clone();
    gesture.connect_drag_begin(move |gesture, _, _| {
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        handle_for_begin.set_cursor_from_name(Some("grabbing"));
    });

    gesture.connect_drag_update({
        let window = window.clone();
        let card = card.clone();
        let card_size = card_size.clone();
        let card_target = card_target.clone();
        move |_, offset_x, offset_y| {
            if window.width() <= 0 || window.height() <= 0 {
                return;
            }
            // The content minimum can make the card larger than what we asked
            // for, so keep whichever is bigger fully on screen.
            let (requested_width, requested_height) = card_size.get();
            let width = card.width().max(requested_width);
            let height = card.height().max(requested_height);
            let max_x = (window.width() - width - SCREEN_EDGE_MARGIN).max(SCREEN_EDGE_MARGIN);
            let max_y = (window.height() - height - SCREEN_EDGE_MARGIN).max(SCREEN_EDGE_MARGIN);

            let origin = (card.margin_start(), card.margin_top());
            let x = (origin.0 + offset_x.round() as i32).clamp(SCREEN_EDGE_MARGIN, max_x);
            let y = (origin.1 + offset_y.round() as i32).clamp(SCREEN_EDGE_MARGIN, max_y);

            if (x, y) == origin {
                return;
            }
            card_target.set((f64::from(x), f64::from(y)));
            card.set_margin_start(x);
            card.set_margin_top(y);
        }
    });

    let handle_for_end = content.title_handle.clone();
    gesture.connect_drag_end(move |_, _, _| {
        handle_for_end.set_cursor_from_name(Some("grab"));
    });

    content.title_handle.add_controller(gesture);
}

/// Drive the card size from drags on the resize grips and persist the result.
///
/// The grips sit at the right/bottom edge, so they move as the card grows.
/// Tracking the raw drag offset would therefore fight itself: the widget shifts
/// out from under the pointer and cancels the very offset driving the resize.
/// The card is positioned by its top-left corner, so pointer coordinates
/// expressed in *card* space are stable, and the size is simply "where the
/// pointer is, plus the distance it was from the edge when the drag began".
fn attach_resize_gestures(
    window: &adw::ApplicationWindow,
    card: &Overlay,
    content: &OverlayContent,
    card_size: &Rc<Cell<(i32, i32)>>,
) {
    for handle in &content.resize_handles {
        let gesture = gtk4::GestureDrag::new();
        let horizontal = handle.horizontal;
        let vertical = handle.vertical;
        // Distance from the pointer to the card edge when the drag started.
        let grab_offset = Rc::new(Cell::new((0.0_f64, 0.0_f64)));

        gesture.connect_drag_begin({
            let window = window.clone();
            let card = card.clone();
            let widget = handle.widget.clone();
            let grab_offset = grab_offset.clone();
            let card_size = card_size.clone();
            move |gesture, start_x, start_y| {
                // Keep the header bar's window handle from taking the sequence.
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                let Some((pointer_x, pointer_y)) =
                    pointer_in_card(&widget, &card, start_x, start_y)
                else {
                    return;
                };
                grab_offset.set((
                    f64::from(card.width()) - pointer_x,
                    f64::from(card.height()) - pointer_y,
                ));

                // Re-anchor to what is on screen: the content minimum may have
                // made the card larger than the size we last asked for.
                if card.width() > 0 && card.height() > 0 {
                    card_size.set(clamp_card_size(
                        card.width(),
                        card.height(),
                        window.width(),
                        window.height(),
                        (card.margin_start(), card.margin_top()),
                    ));
                }
            }
        });

        gesture.connect_drag_update({
            let window = window.clone();
            let card = card.clone();
            let widget = handle.widget.clone();
            let grab_offset = grab_offset.clone();
            let card_size = card_size.clone();
            move |gesture, offset_x, offset_y| {
                let Some((start_x, start_y)) = gesture.start_point() else {
                    return;
                };
                // start_point + offset is the pointer's position in the grip's
                // current coordinate space, which we then map into card space.
                let Some((pointer_x, pointer_y)) =
                    pointer_in_card(&widget, &card, start_x + offset_x, start_y + offset_y)
                else {
                    return;
                };

                let (grab_dx, grab_dy) = grab_offset.get();
                let (last_width, last_height) = card_size.get();
                let requested_width = if horizontal {
                    (pointer_x + grab_dx).round() as i32
                } else {
                    last_width
                };
                let requested_height = if vertical {
                    (pointer_y + grab_dy).round() as i32
                } else {
                    last_height
                };
                let size = clamp_card_size(
                    requested_width,
                    requested_height,
                    window.width(),
                    window.height(),
                    (card.margin_start(), card.margin_top()),
                );

                if card_size.get() == size {
                    return;
                }
                card_size.set(size);
                card.set_size_request(size.0, size.1);
            }
        });

        gesture.connect_drag_end({
            let card_size = card_size.clone();
            let config = content.config.clone();
            move |_, _, _| {
                let (width, height) = card_size.get();

                let mut config = config.borrow_mut();
                if config.window_width == width && config.window_height == height {
                    return;
                }
                config.window_width = width;
                config.window_height = height;
                if let Err(e) = save_config(&config) {
                    warn!("Failed to save window size: {}", e);
                }
            }
        });

        handle.widget.add_controller(gesture);
    }
}

/// Translate a point from a grip's coordinate space into the card's.
fn pointer_in_card(widget: &Box, card: &Overlay, x: f64, y: f64) -> Option<(f64, f64)> {
    let point = gtk4::graphene::Point::new(x as f32, y as f32);
    widget
        .compute_point(card, &point)
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
        let focused = controller
            .widget()
            .and_then(|widget| widget.root())
            .and_then(|root| root.focus());
        let focus_on_row = has_focus_within(&list_box_for_keys)
            && focused.is_none_or(|widget| {
                widget.is::<gtk4::ListBoxRow>() || widget.is::<gtk4::ListBox>()
            });
        let has_shortcut_modifier = state.intersects(
            ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SUPER_MASK,
        );

        match key {
            Key::Delete | Key::KP_Delete if focus_on_row => {
                delete_selected_item(&list_box_for_keys, &history_state_for_keys);
                gtk4::glib::Propagation::Stop
            }
            Key::x | Key::X if focus_on_row && state.contains(ModifierType::CONTROL_MASK) => {
                delete_selected_item(&list_box_for_keys, &history_state_for_keys);
                gtk4::glib::Propagation::Stop
            }
            Key::p | Key::P if focus_on_row && state.contains(ModifierType::CONTROL_MASK) => {
                toggle_pin_selected_item(&list_box_for_keys, &history_state_for_keys);
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

fn delete_selected_item(list_box: &gtk4::ListBox, history_state: &HistoryListState) {
    if let Some(item_id) = selected_item_id(list_box) {
        delete_item(list_box, history_state, item_id);
    }
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
    let neighbour_id = neighbour_item_id(list_box, item_id);

    match FrontendClient::new() {
        Ok(mut client) => {
            if let Err(e) = client.delete_item_by_id(item_id) {
                error!("Error deleting clipboard item by ID: {}", e);
                return;
            }
        }
        Err(e) => {
            error!("Error creating frontend client: {}", e);
            return;
        }
    }

    {
        let mut items = history_state.all_items.borrow_mut();
        if let Some(index) = items.iter().position(|item| item.item_id == item_id) {
            items.remove(index);
        }
    }

    rebuild_list(list_box, history_state);
    match neighbour_id {
        Some(id) => select_row_by_item_id(list_box, history_state, id),
        None => select_first_row(list_box),
    }
}

fn toggle_pin_selected_item(list_box: &gtk4::ListBox, history_state: &HistoryListState) {
    let Some(item_id) = selected_item_id(list_box) else {
        return;
    };
    let Some(pinned) = next_pinned_state(history_state, item_id) else {
        return;
    };

    match FrontendClient::new() {
        Ok(mut client) => {
            if let Err(e) = client.set_pinned(item_id, pinned) {
                error!("Error updating pinned state: {}", e);
                return;
            }
        }
        Err(e) => {
            error!("Error creating frontend client: {}", e);
            return;
        }
    }

    apply_pinned_state(history_state, item_id, pinned);
    rebuild_list(list_box, history_state);
    select_row_by_item_id(list_box, history_state, item_id);
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
    if !icon_theme.has_icon(PIN_ICON) {
        warn!("Bundled icon {PIN_ICON} did not resolve after registering its resource path");
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

    let delete_button = Button::builder().icon_name("user-trash-symbolic").build();
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
        let gbytes = glib::Bytes::from(bytes);
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
        let content_label = Label::new(Some(&item.content_preview));
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

    let list_box = list_box.clone();
    let history_state = history_state.clone();
    let item_id = item.item_id;
    let list_box_for_delete = list_box.clone();
    let history_state_for_delete = history_state.clone();
    delete_button.connect_clicked(move |_| {
        delete_item(&list_box_for_delete, &history_state_for_delete, item_id);
    });
    let list_box_for_pin = list_box.clone();
    let history_state_for_pin = history_state.clone();
    pin_button.connect_clicked(move |_| {
        let Some(pinned) = next_pinned_state(&history_state_for_pin, item_id) else {
            return;
        };

        match FrontendClient::new() {
            Ok(mut client) => {
                if let Err(e) = client.set_pinned(item_id, pinned) {
                    error!("Error updating pinned state: {}", e);
                    return;
                }
            }
            Err(e) => {
                error!("Error creating frontend client: {}", e);
                return;
            }
        }

        apply_pinned_state(&history_state_for_pin, item_id, pinned);
        rebuild_list(&list_box_for_pin, &history_state_for_pin);
        select_row_by_item_id(&list_box_for_pin, &history_state_for_pin, item_id);
        debug!("Updated pinned state for clipboard item ID {}", item_id);
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

fn select_row_by_item_id(
    list_box: &gtk4::ListBox,
    _history_state: &HistoryListState,
    item_id: u64,
) {
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
