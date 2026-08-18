# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`pyroclear` is a terminal `clear` replacement written in Rust. It animates a fire (or a UFO flyby) over the existing terminal content, then wipes the screen **and** scrollback (`\x1b[3J`). The final clear always runs, even on Ctrl-C.

## The defining constraint: zero third-party crates

Beyond `libc` on Unix, there are **no runtime dependencies** — no `clap`, `toml`, `rand`, `crossterm`, `serde`, etc. Everything is hand-rolled:

- **CLI parsing** → manual `std::env::args()` loop in `config::parse_args`.
- **Config / palette files** → line-by-line string parsing of a TOML-like format (NOT real TOML) in `config.rs`.
- **Randomness** → `engine::Rng`, an xorshift64* PRNG seeded from the system clock.
- **Raw mode / terminal size / signals** → direct `libc` calls on Unix (`ioctl(TIOCGWINSZ)`, `termios`, `signal`); hand-rolled Win32 `extern "system"` FFI in `win.rs` on Windows.
- **Rendering** → raw ANSI escape codes only; a single `String` buffer flushed with one `write_all`.

Do not add a crate dependency to solve something that is already done by hand here. If a new dependency is truly warranted, flag the trade-off explicitly.

## Commands

```bash
cargo build --release       # release build (opt-level 3 + LTO, see Cargo.toml)
cargo run --release         # run it
cargo test                  # unit tests (all live in src/ufo.rs)
cargo test crater_is_roughly_circular_and_clipped   # single test by name
cargo clippy --all-targets  # lint — the project keeps clippy clean (see git history)
```

A `Makefile` wraps cargo for cases where the `cargo` proxy is missing from PATH (it falls back to `~/.rustup/toolchains/stable-*/bin`):

```bash
make build        # cargo build --release
make run          # build + run the release binary
make test         # cargo test
make install      # build + copy to BINDIR (default ~/.local/bin), PREFIX overridable
make clean
```

Nix users: `flake.nix` / `package.nix` provide NixOS packaging.

## Architecture

The module layout is documented in the header comment of `src/main.rs` — read it. Entry point is small: `resolve_choice()` → `build_palette()` → dispatch on `settings.effect` to `engine::burn` or `ufo::run` → unconditional final `\x1b[0m[H2J3J`.

### The fire effect (`engine.rs`) — Doom-fire algorithm

- A 2D **heat grid** of `u8`, values `0..=36` (`MAX_HEAT = 36`). One row (bottom by default, top if `direction = true`) is the ignition source set to `MAX_HEAT`.
- Each frame runs `STEPS_PER_FRAME = 2` propagation steps: every cell pulls its new value from the neighbor below (above, if top-down), minus a random **decay** (scaled by `height`) and a random horizontal **drift** (scaled by `wind`, range `-2..=2`). Heat propagates away from the source and cools.
- Heat is mapped to color by indexing a `Palette = [(u8, u8, u8); 37]` directly: `palette[heat as usize]`.
- The source row keeps reigniting until `elapsed > max_duration * flames_duration`, then cools; the loop also ends early once peak heat drops below `DIE_OUT_THRESHOLD`.

### The "transparent overlay then erase" render model (shared by both effects)

This spans `engine.rs` and `ufo.rs` and is the key thing to understand:

1. A **`burned: Vec<bool>`** mask tracks every cell that has *ever* been touched by fire/laser/crater.
2. During animation, only burned/active cells are drawn — **untouched cells are skipped**, so the user's original terminal text shows through until the effect reaches it. This is also how the background stays transparent (cells are left alone rather than painted black; default-bg uses `\x1b[49m`).
3. After the loop, a final pass (`clear_unburned` for fire; implicit for UFO) erases any cell the effect never reached, so nothing remains.

### Single-buffer rendering (no TUI framework)

Both `engine::render` and `ufo::render` build the whole frame into one `String`, then do one `write_all` + `flush`. They batch ANSI state: a cursor move re-emits color, and color codes are only written when the color actually changes. Cursor is hidden (`\x1b[?25l`) on entry and **always** restored on exit (`\x1b[?25h`), including after Ctrl-C. This is why there is no flicker and no double-buffering crate.

### Palettes (`palettes.rs`)

- `FIRE_PALETTE` — the built-in 37-color ramp (the only hand-authored palette).
- `NAMED_PALETTES: &[(&str id, display, desc, from_hex, to_hex)]` — the 300+ entries; each is a two-color gradient generated at runtime.
- The picker is a searchable TUI over `NAMED_PALETTES` (upstream removed `CATEGORIES` and the `--list-colors` flag when the palette list grew past ~700 entries).
- `generate_palette(from, to)` interpolates over 37 steps **through HSV** (hue-aware, `lerp_hue`), then `soften(&raw, SOFTEN_DESATURATE=0.62, SOFTEN_BRIGHTEN=0.32)` is applied to every palette produced by `config::build_palette` (including `--info` previews). When touching color output, apply the same `soften` call or previews won't match the burn.
- `Palette` is 37 entries because heat is `0..=36`.

### Config & CLI resolution (`config.rs`)

`resolve_choice()` is the precedence brain; trace it before changing flag behavior:

1. `load_config()` reads `config.toml` → `(saved_choice, saved_settings)`.
2. `parse_args(&saved_settings)` reads CLI flags → `(parsed_choice, parsed_settings, run_settings, is_reset, parsed_effect)`; the TUI flags (`--pick`, `--custom`) may return overridden `AnimSettings` alongside the palette choice. Some flags (`--pick`, `--custom`, `--start`, `--info`, `--version`, `--help`) print/exit directly from inside the parser.
3. Precedence: `--reset` wins; then `--effect`; then `--settings` (interactive); then an explicit palette choice (`--color`, `--from/--to`, `--random`, or a TUI result); else fall back to the saved/default palette.
4. Every state-changing path persists via `save_config(...)` **unless** `--no-save` is present (`has_no_save()` re-scans `env::args`, since the flag is position-independent).

Config location: `$XDG_CONFIG_HOME/pyroclear/` (fallback `$HOME/.config/pyroclear/`) → `config.toml` + `custom_palettes.toml` (a `[[palette]]` array of name/display/from/to).

### The UFO effect (`ufo.rs`)

Separate simulation sharing the same primitives (`terminal_size`, `Rng`, the burned-mask + cursor-home redraw model). A squadron of saucers enters from the right and sweeps left, firing Bresenham laser lines (`line_cells`) that leave aspect-compensated elliptical craters (`crater_cells`, with `rx = 2 * ry` to look circular on ~2:1 terminal cells) and expanding shockwave rings (`ring_cells`). The pure geometry functions are the **only unit-tested code** in the repo — add tests there when changing geometry.

### Signals (`main.rs`)

A Unix `SIGINT` handler / Windows console-control handler sets a `static AtomicBool`, polled by a watcher thread that mirrors it into the `Arc<AtomicBool>` passed into the effect loop. The loop checks `interrupted` each frame and breaks cleanly, so the terminal is always restored and the final clear always runs.

## Platform gating

The codebase is uniformly split with `#[cfg(unix)]` / `#[cfg(windows)]`. The `libc` dependency is Unix-only (`[target.'cfg(unix)'.dependencies]`), and `win` is declared `#[cfg(windows)]`. `terminal_size()`, raw-mode entry, and signal installation each have both halves — update both when touching terminal handling.

## Conventions

- `pub const ESC: &str = "\x1b"` (in `main.rs`) is used as `{ESC}` in every `write!`/`println!` — don't inline `\x1b`.
- User-facing output (help, info, errors, banners) is heavily ANSI-styled with specific RGB values; match the surrounding style when adding strings.
- `docs/` is gitignored — it holds local session state, not project structure. Don't reference it as part of the codebase.
