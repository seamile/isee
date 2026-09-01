# iSee

A small utility for previewing images in the terminal.

`isee` detects the terminal's image protocol at runtime and renders the image
with the **Kitty graphics protocol** (direct bitmap placement outside tmux,
Unicode placeholder cells inside tmux), **IIP** (iTerm2 OSC 1337 inline file:
Warp, iTerm2, mintty, Tabby, and VSCode — the latter needs
`terminal.integrated.enableImages: true`), or **Sixel** (Foot, Konsole,
Windows Terminal, BlackBox). Terminals without graphics support fall back to
**Half Blocks** (color-reduced character pairs).

> **Platform support** — `isee` is currently built and tested on Linux and
> macOS. Windows is not yet supported.

## Installation

Install with Cargo:

```sh
cargo install isee
```

Or download a prebuilt binary from the
[releases page](https://github.com/seamile/isee/releases).

## Usage

```
isee [OPTIONS] [IMGPATH ...]
```

If `IMGPATH` is omitted, image data is read from `stdin`.

## Options

- `-w WIDTH`: Preview at the given pixel width (e.g. `-w 800` for 800px)
- `-q QUALITY`: Preview scaling quality: `L` (nearest, fastest), `M`
  (triangle, default), `H` (lanczos, sharpest)
- `-p PROTOCOL`: Force the preview protocol: `auto` (default), `kitty`,
  `iip`, `sixel`, or `halfblock` — for terminals whose environment does not
  identify them (e.g. over ssh)
- `-i`: Show image information (size, dimensions, DPI, colorspace, alpha)
- `-a`: Animate GIFs and animated WebPs where the terminal supports it
  (kitty; iTerm2/mintty for GIFs), else fall back to the first frame
- `-v`: Print the version
- `-h, --help`: Print help

Without `-w`, the terminal window width is the only width cap. A preview is
never wider than the terminal window; with `-w` it may be TALLER than the
window — graphics scroll with the text. Only the protocol's hard ceiling
(the tmux kitty placeholder grid) and a 12000 px/side resource guard apply
to height.

## Supported formats

`isee` decodes the following formats. Raster formats go through the Rust
`image` crate; SVG is rasterized with the pure-Rust `resvg`, including gzipped
`.svgz`; HEIC/HEIF on macOS is decoded by the system ImageIO framework:

PNG, JPEG, GIF, WebP, BMP, PNM (PBM/PGM/PPM), QOI, Farbfeld, ICO, TIFF,
Radiance HDR, OpenEXR, SVG, HEIC/HEIF (macOS 10.13+)

Not supported: JPEG2000 needs an external rasterizer outside the `image`
crate; TGA is not content-detectable by `image`; AVIF decoding in `image`
requires the `avif-native` feature and a system libdav1d (its `avif` feature
is encoder-only), so neither renders in `isee`. On Linux, HEIC/HEIF files are
reported as unsupported.

## Examples

```sh
# Preview an image file
isee /foo/bar/image.jpg

# Preview with width 800px
isee -w 800 /foo/bar/image.jpg

# Preview with the sharpest scaling
isee -q H /foo/bar/image.jpg

# Show image information
isee -i /foo/bar/image.jpg

# Animate a GIF or animated WebP (first frame only where the terminal can't)
isee -a /foo/bar/animation.gif
isee -a /foo/bar/animation.webp

# Force a protocol when detection cannot identify the terminal (e.g. ssh)
isee -p sixel /foo/bar/image.jpg
isee -pkitty /foo/bar/image.jpg

# Read image data from a pipe
cat /foo/bar/image.jpg | isee
```

## How it works

- **Protocol detection** — On any tty `isee` sends ONE probe batch, wrapped
  between cursor save/restore and ending with an erase-line that scrubs any
  leaked payload echo. The batch asks the terminal to identify itself
  (XTVERSION) and report its capabilities (DA1; attribute 4 = sixel), plus
  the Kitty graphics query (`\x1b_G...` payload) when the kitty protocol is
  still a candidate. All answers share one deadline. Inside `tmux`, the batch
  is wrapped in tmux's DCS passthrough (`\x1bPtmux;\x1b...\x1b\\`) so the
  *outer* terminal is probed, mirroring how yazi works. The environment brand
  table (`TERM`, `TERM_PROGRAM`, `KITTY_WINDOW_ID`,
  `GHOSTTY_RESOURCES_DIR`, `WEZTERM_EXECUTABLE`, ...) still runs first; an
  XTVERSION self-report overrides it, so terminals whose env vars do not
  survive ssh are still identified. Selection priority: `-p` >
  `ISEE_PROTOCOL` > probed Kitty / kitty env hint > brand table (env or
  XTVERSION) > probed sixel (DA1) > Half Blocks.

- **Scaling & HiDPI** — Bounds are derived from the physical grid cell (via
  `CSI 16 t` and the `TIOCGWINSZ` pixel size), accounting for Retina/HiDPI
  devices-pixels. Without `-w`, the image is shown at its native pixel size
  and shrunk to fit the terminal window; with `-w` it is scaled to that width
  and may end up taller than the window (the terminal scrolls vertically).

- **Bitmap display scale (IIP/Sixel on Retina)** — Bitmap bounds follow the
  terminal's *logical* grid (yazi's convention: one declared px = one
  logical point), and oversized images are auto-fitted to the window. How a
  declared size actually renders is brand-dependent (measured fullscreen on
  a 2x Retina display): Sixel terminals and Warp draw one image px per
  logical point, while iTerm2 draws one image px per *device* pixel and so
  shows the same file twice as wide (isee's bounds stay logical for both
  drivers — exact on Warp, conservative 2x on iTerm2 whose auto-fit clamps
  the result). By default isee declares the image's native pixel size —
  exactly what `imgcat` does, so a 1920x1080 image fills the window and a
  small image shows at its natural point size. `ISEE_DPI_SCALE=2` opts into
  point sizing instead: the bitmap is halved before encoding so a Retina
  screenshot (400x300 px = 200x150 pt) shows at its QuickLook size rather
  than doubled — that is the QuickLook intent on Warp, but on iTerm2 it
  halves again (there, `-w` at twice the width gives QuickLook size);
  `ISEE_DPI_SCALE=1` forces the default explicitly. On scaled displays `-w`
  counts in logical points for these protocols.

- **Kitty rendering** — Outside `tmux` the image is placed directly (`a=T`):
  kitty draws the bitmap at its declared device-pixel size and graphics
  scroll with the text, so an oversized height simply scrolls (only an
  oversized width is truncated at the right edge). The payload travels as a
  temp-file reference (`t=t`) when the terminal supports it, else as chunked
  APC frames, zlib-compressed by default. Inside `tmux`, a grid of Unicode
  placeholder cells is drawn instead (tmux's cursor model cannot track the
  outer terminal's placement moves): only the APC transfer frames are
  wrapped in passthrough; the placeholder cells go through the tmux grid so
  they land in the right pane and survive redraws.

- **IIP rendering** — Terminals speaking iTerm2's OSC 1337 inline-file protocol
  (Warp, iTerm2, mintty, Tabby, and VSCode with
  `terminal.integrated.enableImages: true`) receive the resized image
  base64-encoded in one frame — PNG for alpha images, JPEG q85 otherwise.
  Frames carry no `doNotMoveCursor` (unlike yazi's TUI driver), so the
  terminal advances the cursor below the image exactly as it does for
  `imgcat`, and one newline parks the prompt. Inside `tmux` the frame is
  wrapped in DCS passthrough.
  (Hyper's xterm.js base has no OSC 1337 renderer and gets Half Blocks.)

- **Sixel rendering** — Sixel terminals (Foot, Konsole, Windows Terminal,
  BlackBox) get a Wu-quantized (256-color) sixel bitmap sized with the same
  point-space scale. Inside `tmux`, nested DCS escaping is unreliable, so
  Sixel degrades to Half Blocks there — unless forced with `-p sixel`, which
  rides the DCS passthrough wrapper instead.

- **Animation (`-a`)** — Animated GIFs and animated WebPs play on kitty via
  the native graphics animation protocol (composited full-canvas frames
  transferred as `a=f` chunks); GIF/WebP frames are decoded by the pure-Rust
  `image`/`image-webp` crates. iTerm2/mintty animate GIFs via OSC 1337
  passing the raw file through unmodified for the terminal to play — no
  OSC 1337 terminal renders an animated WebP, so those show the first frame
  there. Everywhere else (Warp, VSCode, Ghostty ≤1.3.1, ...) the first frame
  is shown as a static image, matching `imgcat` behavior. Animation decoding
  is budgeted (192 MiB of target RGBA, 4096 frames) and truncates rather
  than failing.

- **Half Blocks fallback** — Terminals without graphics support get a downscaled,
  color-reduced character-pair rendering. `ISEE_PROTOCOL=half|kitty|iip|sixel`
  overrides detection (`half` is the universal escape hatch); `-p` accepts the
  same values (see Options).

## Acknowledgements

The terminal rendering approach — Unicode placeholder placement, physical-cell
grid sizing, and `tmux` passthrough probing — is heavily inspired by
[yazi](https://github.com/sxyazi/yazi), an excellent terminal file manager.
Thanks to the yazi developers and community for their design and engineering.
