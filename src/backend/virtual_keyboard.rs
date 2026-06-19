use std::fmt::Write as _;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use std::thread::sleep;
use std::time::Duration;

use indexmap::IndexSet;
use rustix::fs::{MemfdFlags, memfd_create};
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

/// Type `text` into the focused window, or synthesize a paste shortcut when
/// `text` is `None`. For the shortcut, `use_shift` sends Ctrl+Shift+V
/// (terminals) instead of plain Ctrl+V (GUI text fields).
pub fn paste_via_virtual_keyboard(text: Option<&str>, use_shift: bool) -> Result<(), String> {
    let connection =
        Connection::connect_to_env().map_err(|e| format!("Wayland connection failed: {e}"))?;
    let (globals, event_queue) =
        registry_queue_init::<VirtualKeyboardState>(&connection).map_err(|e| e.to_string())?;
    let qh = event_queue.handle();

    let seat = globals
        .bind::<WlSeat, _, _>(&qh, 1..=9, ())
        .map_err(|_| "No wl_seat found for virtual keyboard".to_string())?;

    let manager = globals
        .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
        .map_err(|_| "Compositor does not support zwp_virtual_keyboard_manager_v1".to_string())?;

    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());

    if let Some(text) = text {
        type_text(&connection, &keyboard, text)?;
    } else {
        set_keymap(&keyboard, PASTE_KEYMAP)?;
        connection.roundtrip().map_err(|e| e.to_string())?;
        paste_shortcut(&connection, &keyboard, use_shift)?;
    }
    keyboard.destroy();
    connection.roundtrip().map_err(|e| e.to_string())?;
    Ok(())
}

fn set_keymap(keyboard: &ZwpVirtualKeyboardV1, keymap: &[u8]) -> Result<(), String> {
    // Typing keymaps can contain sensitive characters, so keep them in memory.
    let fd = memfd_create(c"cursor-clip-keymap", MemfdFlags::CLOEXEC)
        .map_err(|e| format!("Cannot create keymap: {e}"))?;
    let mut file = File::from(fd);
    file.write_all(keymap)
        .map_err(|e| format!("Cannot write keymap: {e}"))?;
    keyboard.keymap(1, file.as_fd(), keymap.len() as u32);
    Ok(())
}

// Use evdev codes 1..=247. The corresponding XKB codes fit in 8 bits.
const MAX_SYMBOLS: usize = 247;

fn typing_chars(text: &str) -> impl Iterator<Item = char> + Clone {
    let mut chars = text.chars().peekable();
    std::iter::from_fn(move || match chars.next()? {
        '\r' => {
            chars.next_if_eq(&'\n');
            Some('\n')
        }
        ch => Some(ch),
    })
}

fn text_keymap(symbols: &IndexSet<char>) -> String {
    let mut codes =
        String::from("xkb_keymap { xkb_keycodes \"typing\" { minimum=8; maximum=255;\n");
    let mut keys = String::new();
    for (index, ch) in symbols.iter().enumerate() {
        let code = index + 1;
        writeln!(codes, "<K{code}> = {};", code + 8).unwrap();
        write!(keys, "key <K{code}> {{ [ ").unwrap();
        match ch {
            '\n' => keys.push_str("Return"),
            '\t' => keys.push_str("Tab"),
            ch => write!(keys, "U{:04X}", *ch as u32).unwrap(),
        }
        keys.push_str(" ] };\n");
    }
    format!(
        "{codes}}};\nxkb_types \"typing\" {{ include \"complete\" }};\nxkb_compatibility \"typing\" {{}};\nxkb_symbols \"typing\" {{\n{keys}}};\n}};\0"
    )
}

fn next_keymap(chars: impl Iterator<Item = char>) -> (IndexSet<char>, usize) {
    let mut symbols = IndexSet::new();
    let mut count = 0;
    for ch in chars {
        if symbols.len() == MAX_SYMBOLS && !symbols.contains(&ch) {
            break;
        }
        symbols.insert(ch);
        count += 1;
    }
    (symbols, count)
}

fn type_text(
    connection: &Connection,
    keyboard: &ZwpVirtualKeyboardV1,
    text: &str,
) -> Result<(), String> {
    let mut chars = typing_chars(text);
    loop {
        let (symbols, count) = next_keymap(chars.clone());
        if count == 0 {
            return Ok(());
        }
        set_keymap(keyboard, text_keymap(&symbols).as_bytes())?;
        keyboard.modifiers(0, 0, 0, 0);
        for (index, ch) in chars.by_ref().take(count).enumerate() {
            let code = symbols.get_index_of(&ch).unwrap() as u32 + 1;
            keyboard.key(0, code, 1);
            keyboard.key(0, code, 0);
            // Bound the number of queued events for large selections.
            if index % 128 == 127 {
                connection.roundtrip().map_err(|e| e.to_string())?;
            }
        }
    }
}

fn paste_shortcut(
    connection: &Connection,
    keyboard: &ZwpVirtualKeyboardV1,
    use_shift: bool,
) -> Result<(), String> {
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
    connection.roundtrip().map_err(|e| e.to_string())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_repeated_unicode_tabs_and_line_endings_in_order() {
        let chars = typing_chars("A🙂A\t\r\n\r猫");
        let (symbols, count) = next_keymap(chars.clone());
        assert_eq!(count, 7);
        assert_eq!(
            chars
                .map(|ch| symbols.get_index_of(&ch).unwrap() + 1)
                .collect::<Vec<_>>(),
            [1, 2, 1, 3, 4, 4, 5]
        );
        let keymap = text_keymap(&symbols);
        for (code, symbol) in [
            (1, "U0041"),
            (2, "U1F642"),
            (3, "Tab"),
            (4, "Return"),
            (5, "U732B"),
        ] {
            assert!(keymap.contains(&format!("key <K{code}> {{ [ {symbol} ] }};")));
        }
        assert!(keymap.ends_with('\0'));
    }

    #[test]
    fn full_keymap_is_reused_until_a_new_symbol_is_needed() {
        let unique = (0x4e00..).filter_map(char::from_u32).take(MAX_SYMBOLS);
        let mut chars = unique.chain("一🙂".chars());
        let (symbols, count) = next_keymap(chars.clone());
        assert_eq!(symbols.len(), MAX_SYMBOLS);
        assert_eq!(count, MAX_SYMBOLS + 1);
        assert_eq!(chars.nth(count), Some('🙂'));
    }
}
