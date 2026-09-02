# iSee

An image viewer for terminal, simple and fast.

**iSee** supports the [**Kitty Graphics Protocol**](https://sw.kovidgoyal.net/kitty/graphics-protocol/), [**iTerm Inline Images Protocol**](https://iterm2.com/documentation-images.html), [**Sixel Protocol**](https://en.wikipedia.org/wiki/Sixel), and **ANSI Half-Block** rendering.

`isee` detects the running terminal's image protocol at runtime and renders the image with the best option the terminal supports — one of the three bitmap protocols below, or plain-text **Half Blocks** when no bitmap protocol is available. `-p PROTOCOL` forces a specific protocol when detection cannot identify the terminal (see Options).

**Supported protocols**

- **Kitty Graphics Protocol (KGP)** — direct bitmap placement at the declared pixel size; the fastest option, preferred wherever the terminal supports it.
- **iTerm Inline Images Protocol (IIP)** — the image, base64-encoded in a single OSC 1337 frame, rides the text stream.
- **Sixel** — 256-color DCS bitmaps.
- **Half Blocks** — color-reduced character pairs; the fallback for terminals with no bitmap protocol at all (e.g. Hyper).

**Supported terminals**

Detection reads the environment's brand table; terminals it does not know are probed for their XTVERSION self-report, so even terminals whose env vars do not survive ssh are identified. Recognized terminals map to a protocol as follows.

- **KGP**: kitty, Ghostty, WezTerm, iTerm2, Warp
- **IIP**: mintty, Tabby, Bobcat, VS Code (VS Code needs `terminal.integrated.enableImages: true` — off by default)
- **Sixel**: Foot, Konsole, Windows Terminal, BlackBox
- **Half Blocks**: Hyper — it supports no bitmap protocol at all

> **Platform support** — `isee` is currently built and tested on Linux and macOS. Windows is not yet supported.

## Installation

Install with Cargo:

```sh
cargo install isee
```

Or download a prebuilt binary from the [releases page](https://github.com/seamile/isee/releases).

## Usage

```
isee [OPTIONS] [IMGPATH ...]
```

If `IMGPATH` is omitted, image data is read from `stdin`.

## Options

- `-w WIDTH`: Preview at the given pixel width (e.g. `-w 800` for 800px)
- `-q QUALITY`: Preview scaling quality: `L` (nearest, fastest), `M` (triangle, default), `H` (lanczos, sharpest)
- `-p PROTOCOL`: Force the preview protocol: `auto` (default), `kitty`, `iip`, `sixel`, or `halfblock` — for terminals whose environment does not identify them (e.g. over ssh)
- `-i`: Show image information (size, dimensions, DPI, colorspace, alpha)
- `-a`: Animate GIFs and animated WebPs where the terminal supports it (kitty; iTerm2/mintty for GIFs), else fall back to the first frame
- `-v`: Print the version
- `-h, --help`: Print help

Without `-w`, the terminal window width is the only width cap. A preview is never wider than the terminal window; with `-w` it may be TALLER than the window — graphics scroll with the text. Only the protocol's hard ceiling (the tmux kitty placeholder grid) and a 12000 px/side resource guard apply to height.

## Supported formats

`isee` decodes the following formats. Raster formats go through the Rust `image` crate; SVG is rasterized with the pure-Rust `resvg`, including gzipped `.svgz`; HEIC/HEIF on macOS is decoded by the system ImageIO framework:

PNG, JPEG, GIF, WebP, BMP, PNM (PBM/PGM/PPM), QOI, Farbfeld, ICO, TIFF, Radiance HDR, OpenEXR, SVG, HEIC/HEIF (macOS 10.13+)

Not supported: JPEG2000 needs an external rasterizer outside the `image` crate; TGA is not content-detectable by `image`; AVIF decoding in `image` requires the `avif-native` feature and a system libdav1d (its `avif` feature is encoder-only), so neither renders in `isee`. On Linux, HEIC/HEIF files are reported as unsupported.

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

- **Protocol detection** — On a tty, detection first consults the environment brand table (`TERM`, `TERM_PROGRAM`, `KITTY_WINDOW_ID`, `GHOSTTY_RESOURCES_DIR`, `WEZTERM_EXECUTABLE`, ...); a brand hit already carries the protocol verdict from the measured support matrix and skips the probe entirely. Only a forced `-p kitty` (which still needs the tempfile-transport confirmation) or a terminal the brand table does not know runs the probe batch: XTVERSION (brand self-report), DA1 (attribute 4 = sixel), and the KGP query, all sharing one deadline, wrapped between cursor save/restore and ending with an erase-line that scrubs any leaked payload echo — so unknown terminals (whose env vars do not survive ssh) are still identified. Inside `tmux` the batch rides the DCS passthrough wrapper so the *outer* terminal is probed, mirroring yazi. Selection priority: `-p` > `ISEE_PROTOCOL` > brand table (env, or XTVERSION when the probe ran) > probed KGP / kitty env hint > probed Sixel (DA1) > Half Blocks.

- **Scaling & HiDPI** — Bounds are derived from the physical grid cell (via `CSI 16 t` and the `TIOCGWINSZ` pixel size), accounting for Retina/HiDPI device pixels. Without `-w`, the image is shown at its native pixel size and shrunk to fit the terminal window; with `-w` it is scaled to that width and may end up taller than the window (the terminal scrolls vertically).

- **Bitmap display scale (IIP/Sixel on Retina)** — Bitmap bounds are in device pixels (the physical grid cell), so these protocols address the full window on HiDPI displays. By default isee declares the image's native pixel size and lets the terminal auto-fit anything oversized — what `imgcat` does. How one declared px renders is brand-dependent (measured on a 2x Retina display): Warp draws one px per logical point, iTerm2 and Ghostty one per *device* pixel, so the same file shows 2x wider on Warp. `ISEE_DPI_SCALE=2` opts into point sizing: the bitmap is halved before encoding, so a Retina screenshot (400x300 px = 200x150 pt) shows at QuickLook size on Warp — iTerm2 halves it again (there `-w` at twice the width gives QuickLook size); `ISEE_DPI_SCALE=1` forces the default. KGP and Half Blocks render in device/cell pixels and ignore `ISEE_DPI_SCALE`.

- **KGP rendering** — Outside `tmux` the image is placed directly (`a=T`): kitty draws the bitmap at its declared device-pixel size and graphics scroll with the text, so an oversized height simply scrolls (only an oversized width is truncated at the right edge). The payload travels as a temp-file reference (`t=t`) when the terminal supports it, else as chunked APC frames, zlib-compressed by default. Inside `tmux`, a grid of Unicode placeholder cells is drawn instead (tmux's cursor model cannot track the outer terminal's placement moves): only the APC transfer frames are wrapped in passthrough; the placeholder cells go through the tmux grid so they land in the right pane and survive redraws.

- **IIP rendering** — The resized image is base64-encoded into one OSC 1337 inline-file frame — PNG for alpha images, JPEG q85 otherwise. Frames carry no `doNotMoveCursor` (unlike yazi's TUI driver), so the terminal advances the cursor below the image exactly as it does for `imgcat`, and one newline parks the prompt. Inside `tmux` the frame is wrapped in DCS passthrough.

- **Sixel rendering** — Sixel terminals (Foot, Konsole, Windows Terminal, BlackBox) get a Wu-quantized (256-color) sixel bitmap sized with the same point-space scale. Inside `tmux`, nested DCS escaping is unreliable, so Sixel degrades to Half Blocks there — unless forced with `-p sixel`, which rides the DCS passthrough wrapper instead.

- **Animation (`-a`)** — Animated GIFs and animated WebPs play on kitty via the native graphics animation protocol (composited full-canvas frames transferred as `a=f` chunks); GIF/WebP frames are decoded by the pure-Rust `image`/`image-webp` crates. iTerm2/mintty animate GIFs via OSC 1337 passing the raw file through unmodified for the terminal to play — no OSC 1337 terminal renders an animated WebP, so those show the first frame there. Everywhere else (Warp, VS Code, Ghostty ≤1.3.1, ...) the first frame is shown as a static image, matching `imgcat` behavior. Animation decoding is budgeted (192 MiB of target RGBA, 4096 frames) and truncates rather than failing.

- **Half Blocks fallback** — Terminals without graphics support get a downscaled, color-reduced character-pair rendering. `ISEE_PROTOCOL=half|kitty|iip|sixel` overrides detection (`half` is the universal escape hatch); `-p` accepts the same values (see Options).

## Acknowledgements

The terminal rendering approach — Unicode placeholder placement, physical-cell grid sizing, and `tmux` passthrough probing — is heavily inspired by [yazi](https://github.com/sxyazi/yazi), an excellent terminal file manager. Thanks to the yazi developers and community for their design and engineering.
