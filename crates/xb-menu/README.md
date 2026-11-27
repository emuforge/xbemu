# xb-menu

The core's in-game settings menu: what the pad does to it and the picture it
draws. Layer L1; depends only on `xb-types`.

## What it owns

- Detecting the open/close combo, when `Settings::menu_combo` names one,
  and saying which buttons to hide from the title.
- Menu navigation: selection, value changes, the confirm step for "Reset game"
  and "Quit", auto-repeat.
- The item table (`src/items.rs`), which navigation and drawing both walk.
- Drawing the menu and the one-line HUD into a caller-provided RGBA8 buffer,
  with the embedded 8x8 font.

It does not own the settings' meaning, their persistence, pausing, or
compositing: `xb-core` applies an `Outcome`, and the GPU side blends the
overlay.

## Invariants

- **Pure.** No I/O, no clock, no graphics API, no logging. Time arrives as the
  `dt` argument of `Menu::update`.
- **`unsafe` is not allowed** (`#![forbid(unsafe_code)]`).
- **No allocation** in `update`, `render` or `draw_hud`. Status strings are
  copied into fixed 40-byte buffers.
- **Drawing never leaves the buffer**, for any canvas size including 0x0.
- **Straight alpha.** Canvas bytes are R, G, B, A, top row first, stride
  `width * 4`, color not premultiplied. Blend with `SRC_ALPHA` /
  `ONE_MINUS_SRC_ALPHA`. Details in `src/canvas.rs`.
- **`render` writes every pixel** of the canvas, so the buffer needs no
  clearing, and clears `is_dirty`. `is_dirty` turns true only when the picture
  would differ.
- One `Outcome` per `update`. When two inputs arrive in the same frame the
  order is: combo, Start, B, A, direction.
- The menu never changes `Settings::menu_combo`.

## Behavior

**The combo is opt-in.** `Settings::menu_combo` of 0, the default, means no
combo: the front end opens and closes the menu with `Menu::open` and
`Menu::close` from a button the console's pad does not have (the standalone
app uses the Guide button and F1). A front end with no such button sets a
mask of console buttons.

**The combo must be held for half a second to open** (`COMBO_HOLD`). A
combo is made of buttons titles use. With both stick clicks, in Halo the left
click crouches while held and the right click zooms, so crouching and zooming
presses both for a moment. A hold longer than such an overlap avoids opening
by accident. During the hold the title still sees the buttons, since until
the hold completes they are ordinary game input. Closing with the combo is
immediate, as it has no other meaning while the menu is up.

**One trigger per press.** A pad arms when none of its combo buttons is down
and disarms when the combo triggers, so holding on does nothing more, and
letting go of one button and pressing it again is not a new press. A combo
already held at the first update does nothing. The whole combo must be on one
pad; any of the four ports will do.

**Buttons stay hidden until released.** When the menu opens or closes, every
button down at that moment (the combo's, or the A that chose "Resume", or
Start) is withheld from the title until it is released, so no press leaks
into the game. That covers the analog buttons too, which is why `filter_pad`
is the recommended interface and `hidden_buttons` (digital only) is not
enough on its own. Hidden buttons are also ignored by navigation, so a custom
combo containing Start or the D-pad does not close or scroll the menu it just
opened.

**Left and right stop at the ends; A wraps.** A slider that wraps from 100%
to 0% under a held D-pad is a nuisance, while A is the one-button way to
cycle. On/off rows flip with any of the three.

**"Next boot" is a mark, not a mode.** `Capabilities` says which settings
wait for the next boot. Those rows get an amber `*` and the list a one-line
legend. They are edited and reported like any other; the core decides what to
do with the change.

**Reopening starts on "Resume".** The selection is not remembered, so
opening the menu and pressing A always returns to the game.

**The font** is the public-domain `font8x8` basic set by Daniel Hepper,
derived from Marcel Sondaar's public-domain IBM VGA font listing, with the
capital `V` redrawn. Source and license are stated in `src/font.rs`.

## Adding an item

Add a `Setting` variant and one `Item` to `ITEMS` in `src/items.rs`: a label,
a `Values` (`Toggle`, `Choice(&[..])` or `Range {..}`), a getter and a setter.
The layout has room for 14 rows; a compile-time assertion in `src/render.rs`
fails if the list outgrows the panel.

## Test

```
cargo test -p xb-menu
```

To look at the result:

```
cargo run -p xb-menu --example preview -- target/menu-preview
```

writes PPM files of the menu in several states, blended over a test pattern.
