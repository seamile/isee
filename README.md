# iSee

A small utility for previewing images in the terminal.

`isee` detects the terminal's image protocol at runtime and renders the image
with either the **Kitty graphics protocol** (with Unicode placeholder cells for
precise sizing) or **Half Blocks** (color-reduced character pairs) as a fallback
for terminals without graphics support.

> **Platform support** — `isee` is currently built and tested on Linux and
> macOS. Windows is not yet supported.

## Usage

```
isee [OPTIONS] [IMGPATH]
```

If `IMGPATH` is omitted, image data is read from `stdin`.

## Options

- `-w WIDTH`: Preview at the given pixel width (e.g. `-w 800` for 800px)
- `-q QUALITY`: Preview quality, 0–100 (e.g. `-q 80` for 80%)
- `-i`: Show image information (size, dimensions, DPI, colorspace, alpha)
- `-v, --version`: Print the version
- `-h, --help`: Print help

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

# Preview with quality 80%
isee -q 80 /foo/bar/image.jpg

# Show image information
isee -i /foo/bar/image.jpg

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

- **Kitty rendering** — The image is uploaded as a sequence of 4096-byte chunked
  APC frames. A grid of Unicode placeholder cells is then drawn at the exact
  image size, giving crisp, correctly-positioned previews. Inside `tmux`, only
  the APC transfer frames are wrapped in passthrough; the placeholder cells go
  through the tmux grid so they land in the right pane and survive redraws.

- **Half Blocks fallback** — Terminals without graphics support get a downscaled,
  color-reduced character-pair rendering.

## Acknowledgements

The terminal rendering approach — Unicode placeholder placement, physical-cell
grid sizing, and `tmux` passthrough probing — is heavily inspired by
[yazi](https://github.com/sxyazi/yazi), an excellent terminal file manager.
Thanks to the yazi developers and community for their design and engineering.
