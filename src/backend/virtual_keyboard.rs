use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::AsFd;
use std::thread::sleep;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

const PASTE_KEYMAP: &[u8] = b"xkb_keymap {\n\
xkb_keycodes \"(unnamed)\" {\n\
minimum = 8;\n\
maximum = 12;\n\
<K1> = 9;\n\
<K2> = 10;\n\
<K3> = 11;\n\
};\n\
xkb_types \"(unnamed)\" { include \"complete\" };\n\
xkb_compatibility \"(unnamed)\" { include \"complete\" };\n\
xkb_symbols \"(unnamed)\" {\n\
key <K1> {[Control_L]};\n\
key <K2> {[v, V]};\n\
key <K3> {[Shift_L]};\n\
};\n\
};\n\0";

// XKB default modifier mask bits.
const MOD_SHIFT: u32 = 1;
const MOD_CONTROL: u32 = 4;

struct VirtualKeyboardState;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for VirtualKeyboardState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(VirtualKeyboardState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(VirtualKeyboardState: ignore ZwpVirtualKeyboardV1);
delegate_noop!(VirtualKeyboardState: ignore WlSeat);

/// Synthesize a paste shortcut. When `use_shift` is true sends Ctrl+Shift+V
/// (terminals), otherwise plain Ctrl+V (GUI text fields).
pub fn paste_via_virtual_keyboard_shortcut(use_shift: bool) -> Result<(), String> {
    let connection =
        Connection::connect_to_env().map_err(|e| format!("Wayland connection failed: {e}"))?;
    let (globals, mut event_queue) =
        registry_queue_init::<VirtualKeyboardState>(&connection).map_err(|e| e.to_string())?;
    let qh = event_queue.handle();

    let seat = globals
        .bind::<WlSeat, _, _>(&qh, 1..=9, ())
        .map_err(|_| "No wl_seat found for virtual keyboard".to_string())?;

    let manager = globals
        .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
        .map_err(|_| "Compositor does not support zwp_virtual_keyboard_manager_v1".to_string())?;

    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "cursor-clip-keymap-{}-{}.xkb",
        std::process::id(),
        nanos
    ));

    let mut keymap_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .read(true)
        .open(&path)
        .map_err(|e| format!("Failed to create temporary keymap file: {e}"))?;

    keymap_file
        .write_all(PASTE_KEYMAP)
        .map_err(|e| format!("Failed to write keymap: {e}"))?;
    keymap_file
        .flush()
        .map_err(|e| format!("Failed to flush keymap: {e}"))?;

    keyboard.keymap(1, keymap_file.as_fd(), PASTE_KEYMAP.len() as u32);

    let mut vk_state = VirtualKeyboardState;
    event_queue
        .roundtrip(&mut vk_state)
        .map_err(|e| format!("Wayland roundtrip failed: {e}"))?;

    // Press Ctrl (and optionally Shift), declare modifiers, tap V, then clear
    // modifiers and release the held keys. Some clients only honor modifier
    // combinations when the modifier state is sent explicitly.
    // Keycodes: 1 = Control_L (<K1>), 2 = v (<K2>), 3 = Shift_L (<K3>).
    let mods = if use_shift {
        MOD_CONTROL | MOD_SHIFT
    } else {
        MOD_CONTROL
    };

    keyboard.key(0, 1, 1);
    if use_shift {
        keyboard.key(0, 3, 1);
    }
    keyboard.modifiers(mods, 0, 0, 0);
    connection
        .flush()
        .map_err(|e| format!("Failed to flush modifier keys down: {e}"))?;
    sleep(Duration::from_millis(10));

    keyboard.key(0, 2, 1);
    connection
        .flush()
        .map_err(|e| format!("Failed to flush V down: {e}"))?;
    sleep(Duration::from_millis(6));

    keyboard.key(0, 2, 0);
    connection
        .flush()
        .map_err(|e| format!("Failed to flush V up: {e}"))?;
    sleep(Duration::from_millis(6));

    keyboard.modifiers(0, 0, 0, 0);
    if use_shift {
        keyboard.key(0, 3, 0);
    }
    keyboard.key(0, 1, 0);
    connection
        .flush()
        .map_err(|e| format!("Failed to flush virtual keyboard shortcut: {e}"))?;

    // Let the compositor process the key-up events before tearing the keyboard
    // down. Destroying immediately after a bare flush can truncate the sequence
    // on some compositors (e.g. cosmic-comp), so settle and round-trip first.
    sleep(Duration::from_millis(20));
    let _ = event_queue.roundtrip(&mut vk_state);

    keyboard.destroy();
    let _ = std::fs::remove_file(path);
    let _ = connection.flush();

    Ok(())
}
