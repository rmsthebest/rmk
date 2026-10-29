//! The firmware's action hook, which a scenario file cannot set.

use core::sync::atomic::{AtomicBool, Ordering};

use rmk::event::KeyboardEvent;
use rmk::hid::Report;
use rmk::keyboard::set_action_hook;
use rmk::test_support::test_block_on;
use rmk::types::action::Action;
use rmk::types::keycode::{HidKeyCode, KeyCode};
use rmk::types::modifier::ModifierCombination;
use rmk::{k, user};
use usbd_hid::descriptor::MediaKeyboardReport;

use crate::simulator::SimKeyboard;

/// The consumer usage of AudioMute.
const MUTE: u16 = 0xE2;

fn mute(usage_id: u16) -> Report {
    Report::MediaKeyboardReport(MediaKeyboardReport { usage_id })
}

static SWAP: AtomicBool = AtomicBool::new(false);

/// Turns A into Mute while `SWAP` is on; User(0) toggles it.
fn swap_hook(action: &mut Action, event: &KeyboardEvent, _: ModifierCombination) -> bool {
    match *action {
        Action::User(0) => {
            if event.pressed {
                SWAP.fetch_xor(true, Ordering::Relaxed);
            }
            false
        }
        Action::Key(KeyCode::Hid(HidKeyCode::A)) if SWAP.load(Ordering::Relaxed) => {
            *action = Action::Key(KeyCode::Hid(HidKeyCode::AudioMute));
            true
        }
        _ => true,
    }
}

/// A key's release ends what the hook made of its press, though the hook
/// would rewrite it differently by the release: a keyboard key released as a
/// consumer key, or the other way round, would stick.
#[test]
fn a_release_ends_what_the_hook_made_of_the_press() {
    test_block_on(async {
        set_action_hook(swap_hook);
        let mut keyboard = SimKeyboard::builder([[[k!(A), user!(0)]]]).build().await;
        keyboard
            .press(0, 0)
            .expect_keys([HidKeyCode::A])
            .tap(0, 1, 10)
            .release(0, 0)
            .expect_keys([])
            .press(0, 0)
            .expect_report(mute(MUTE))
            .tap(0, 1, 10)
            .release(0, 0)
            .expect_report(mute(0));
        keyboard.run().await;
    });
}
