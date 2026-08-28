# iSee

A small utility for previewing images in the terminal.

`isee` detects the terminal's image protocol at runtime and renders the image
with the **Kitty graphics protocol** (with Unicode placeholder cells for
precise sizing), **Iip** (iTerm2 OSC 1337 inline file: Warp, iTerm2, mintty,
Tabby, and VSCode — the latter needs `terminal.integrated.enableImages: true`),
or **Sixel** (Foot, Konsole, Windows Terminal,
BlackBox). Terminals without graphics support fall back to **Half Blocks**
(color-reduced character pairs).

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
- `-i`: Show image information (size, dimensions, DPI, colorspace, alpha)
- `-a`: Animate GIFs where the terminal supports it (kitty; iTerm2, mintty),
  else fall back to the first frame
- `-v`: Print the version
- `-h, --help`: Print help

Without `-w`, previews are capped at 1920px wide; a preview is never wider
than the terminal window.

## Supported formats

`isee` decodes the following formats. Raster formats go through the Rust
`image` crate; SVG is rasterized with the pure-Rust `resvg`, including gzipped
`.svgz`:

PNG, JPEG, GIF, WebP, BMP, PNM (PBM/PGM/PPM), QOI, Farbfeld, ICO, TIFF,
Radiance HDR, OpenEXR, SVG

Not supported: JPEG2000 and HEIC need external rasterizers/decoders outside
the `image` crate; TGA is not content-detectable by `image`, and pure Rust
AVIF support is encoder-only, so neither renders in `isee`.

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

# Animate a GIF (first frame only where the terminal can't animate)
isee -a /foo/bar/animation.gif

# Read image data from a pipe
cat /foo/bar/image.jpg | isee
```

## How it works

- **Protocol detection** — `isee` probes the running terminal with the Kitty
  graphics query (`\x1b_G...` payload) and reads the cell-size query (`CSI 16 t`).
  When inside `tmux`, queries are wrapped in tmux's DCS passthrough
  (`\x1bPtmux;\x1b...\x1b\\`) so the *outer* terminal is probed, mirroring how
  yazi works. Detection is gated on environment signals (`TERM`, `TERM_PROGRAM`,
  `KITTY_WINDOW_ID`, `GHOSTTY_RESOURCES_DIR`, `WEZTERM_EXECUTABLE`) so the probe
  sequence is never emitted to a terminal that cannot consume it.

- **Scaling & HiDPI** — Bounds are derived from the physical grid cell (via
  `CSI 16 t` and the `TIOCGWINSZ` pixel size), accounting for Retina/HiDPI
  devices-pixels. Without `-w`, the image is shown at its native pixel size and
  only shrunk to fit the terminal; with `-w` it is scaled to that width.

- **Bitmap display scale (Iip/Sixel on Retina)** — OSC 1337 and Sixel sizes
  are declared in *logical points* (the terminal renders 1px = 1pt and
  auto-fits oversized images to the window). By default isee declares the
  image's native pixel size — exactly what `imgcat` does, so a 1920x1080
  image fills the window and a small image shows at its natural point size.
  `ISEE_DPI_SCALE=2` opts into point sizing instead: the bitmap is halved
  before encoding so a Retina screenshot (400x300 px = 200x150 pt) shows at
  its QuickLook size rather than doubled; `ISEE_DPI_SCALE=1` forces the
  default explicitly. On scaled displays `-w` counts in logical points for
  these protocols.

- **Kitty rendering** — The image is uploaded as a sequence of 4096-byte chunked
  APC frames. A grid of Unicode placeholder cells is then drawn at the exact
  image size, giving crisp, correctly-positioned previews. Inside `tmux`, only
  the APC transfer frames are wrapped in passthrough; the placeholder cells go
  through the tmux grid so they land in the right pane and survive redraws.

- **Iip rendering** — Terminals speaking iTerm2's OSC 1337 inline-file protocol
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
  point-space scale. Inside `tmux`, nested
  DCS escaping is unreliable, so Sixel degrades to Half Blocks there; force it
  with `ISEE_PROTOCOL=sixel` if you know your setup works.

- **GIF animation (`-a`)** — Animated GIFs play on terminals that support
  animation: kitty via the native graphics animation protocol (composited
  full-canvas frames transferred as `a=f` chunks), and iTerm2/mintty via OSC
  1337 passing the raw GIF through unmodified for the terminal to play.
  Everywhere else (Warp, VSCode, Ghostty ≤1.3.1, ...) the first frame is
  shown as a static image, matching `imgcat` behavior. Animation decoding is
  budgeted (192 MiB of target RGBA, 4096 frames) and truncates rather than
  failing.

- **Half Blocks fallback** — Terminals without graphics support get a downscaled,
  color-reduced character-pair rendering. `ISEE_PROTOCOL=half|kitty|iip|sixel`
  overrides detection (`half` is the universal escape hatch).

## Acknowledgements

The terminal rendering approach — Unicode placeholder placement, physical-cell
grid sizing, and `tmux` passthrough probing — is heavily inspired by
[yazi](https://github.com/sxyazi/yazi), an excellent terminal file manager.
Thanks to the yazi developers and community for their design and engineering.
