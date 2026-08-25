# iSee
A utility for previewing images in the terminal.

## Usage

```
isee [OPTIONS] IMGPATH
```

### Options

| Option       | Description                                            |
|--------------|--------------------------------------------------------|
| `-w WIDTH`   | Preview with the given width (e.g. `-w 800` for 800px) |
| `-q QUALITY` | Preview with the given quality (e.g. `-q 80` for 80%)  |
| `-i`         | Show image information                                 |

### Examples

```sh
# Preview an image file
isee img_path

# Preview with width 800px
isee -w 800 img_path

# Preview with quality 80%
isee -q 80 img_path

# Show image information
isee -i img_path

# Read image data from a pipe
cat img_path | isee
curl img_url | isee
```
