//! Default-layer keys under the QMK default-layer mode, which a scenario file
//! cannot turn on.

use rmk::keymap::set_qmk_default_layer;
use rmk::test_support::test_block_on;
use rmk::types::keycode::HidKeyCode;
use rmk::{a, df, k, mo};

use crate::simulator::SimKeyboard;

/// With QMK's default layer, setting it turns the other layers off. That
/// happens on the press: releasing DF(1) leaves a layer held since alone.
#[test]
fn a_default_layer_release_keeps_a_layer_held_since() {
    test_block_on(async {
        set_qmk_default_layer(true);
        let mut keyboard = SimKeyboard::builder([
            [[df!(1), mo!(2), k!(A)]],
            [[df!(1), mo!(2), k!(B)]],
            [[a!(Transparent), a!(Transparent), k!(C)]],
        ])
        .build()
        .await;
        keyboard
            .press(0, 0)
            .press(0, 1)
            .release(0, 0)
            .press(0, 2)
            .expect_keys([HidKeyCode::C])
            .release(0, 2)
            .expect_keys([])
            .release(0, 1);
        keyboard.run().await;
        set_qmk_default_layer(false);
    });
}
