# rmk main, with the Akko 5075B Plus patches

This branch (`akko-main`) is rmk's `main` at 99aa1386d (2026-10-28), plus
the commits below. The Akko 5075B Plus firmware
(https://github.com/rmsthebest/rmk-5075b-plus) uses it as the submodule
`third_party/rmk`. Each commit is written to stand alone as an upstream
pull request. The older 0.9.0-based patches are on `akko-0.9`.

Hooks for things RMK doesn't know about (the board's radio module, its
lighting, its DFU entry):

- **`RMK_COMMIT` from the environment** (`build.rs`). The storage schema
  hash includes rmk's last commit, so every commit on a fork erased the
  stored keymap. The firmware pins `RMK_COMMIT` in its
  `.cargo/config.toml`. The hash still covers rmk's version, features and
  macro/combo/morse sizes.
- **VIA custom values** (`host::set_custom_value_hook`): the firmware
  answers id_custom_set_value, get_value and save (0x07-0x09). The board
  uses them for Vial's lighting tab (VialRGB) and its diagnostic channels.
- **Action hook** (`keyboard::set_action_hook`): sees each action before
  the keyboard processes it. It can replace the action (Right Ctrl as App)
  or keep it from the keyboard (the lighting's recording mode, EE_CLR's
  3 s hold).
- **QMK magic keycodes** (`host/via/keycode_convert.rs`): 0x7000-0x701F
  (GU_TOGG and the rest) map to and from `Action::User(0x80 | n)`, not No.
- **Report hook** (`channel::set_report_hook`, `channel::release_usb_reports`,
  `hid::set_host_leds`): the firmware takes reports for its UART radio
  module and leaves USB an all-up report. The module's lock LEDs come in as
  an LED OUT report would.
- **Bootloader hook** (`boot::set_bootloader_hook`): VIA's bootloader jump
  goes into the board's ROM DFU.
- **USB battery report** (`hid::set_battery`, `hid::set_battery_now`):
  report id 0x05 on the composite interface (Battery Strength and
  Charging), so the battery shows in upower. It's sent only while
  configured and dropped after 20 ms. GET_REPORT answers 100% until the
  level is known, because upower ignores a battery whose first read fails.

Fixes and performance (also on their own branches for upstream PRs):

- **fix(via):** `DynamicKeymapSetBuffer` reads keycodes as `GetBuffer`
  writes them. Before, it read past the end of the report (a panic).
- **perf(keyboard):** an insertion sort for the held buffer, in place of
  `sort_unstable_by_key`, which brought a 1.6 KB stack frame and about
  8 KB of code.

Dropped from `akko-0.9`, because main already has them: the storage page
cache, the cache reset after `erase_all`, and the old build hash
(`RMK_BUILD_HASH`).
