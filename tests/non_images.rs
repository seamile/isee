use std::io::Write;
use std::process::{Command, Output, Stdio};

const IMAGE: &str = "tests/fixtures/anim.webp";
const MODES: &[&[&str]] = &[
    &["-p", "halfblock"],
    &["-p", "halfblock", "-c", "0,0,1,1"],
    &["-p", "halfblock", "-a"],
    &["-p", "halfblock", "-t3"],
    &["-i"],
];

fn command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_isee"));
    command
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(args)
        .env_remove("TMUX")
        .env("TERM_PROGRAM", "")
        .env("ISEE_KGP_TRANSFER", "stream");
    command
}

fn assert_skipped(output: &Output, paths: &[&str]) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr.lines().count(), paths.len(), "{stderr}");
    for path in paths {
        assert!(
            stderr.contains(&format!("isee: {path}: not a supported image; skipping")),
            "{stderr}"
        );
    }
}

#[test]
fn non_images_only_warn_without_output_in_every_mode() {
    let paths = ["tests", "tests/fixtures", "README.md", "Cargo.toml"];
    for mode in MODES {
        for targets in paths.chunks(1).chain([paths.as_slice()]) {
            let output = command(mode).args(targets).output().unwrap();
            assert_skipped(&output, targets);
            assert!(output.stdout.is_empty());
        }
    }
}

#[test]
fn non_images_do_not_leave_titles_gaps_or_extra_rows() {
    for mode in MODES {
        let baseline = command(mode)
            .args([IMAGE, "tests/fixtures/small.avif", IMAGE, IMAGE])
            .output()
            .unwrap();
        assert!(baseline.status.success());
        let output = command(mode)
            .args([
                "tests",
                IMAGE,
                "README.md",
                "tests/fixtures/small.avif",
                IMAGE,
                "Cargo.toml",
                IMAGE,
                "tests/fixtures",
            ])
            .output()
            .unwrap();
        assert_skipped(
            &output,
            &["tests", "README.md", "Cargo.toml", "tests/fixtures"],
        );
        assert_eq!(output.stdout, baseline.stdout, "mode: {mode:?}");
    }
}

fn stdin(args: &[&str], data: &[u8]) -> Output {
    let mut child = command(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(data).unwrap();
    child.wait_with_output().unwrap()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(data).unwrap();
    gzip.finish().unwrap()
}

#[test]
fn non_svg_xml_and_unidentified_bad_gzip_are_skipped() {
    let inputs: &[&[u8]] = &[
        b"<?xml version=\"1.0\"?><svg xmlns=\"urn:example\" width=\"1\" height=\"1\"/>",
        b"<?xml version=\"1.0\"?><document><a></document>",
        b"<?xml version=\"1.0\"?><document xmlns:x=\"http://www.w3.org/2000/svg\"><a></document>",
        b"\x1f\x8bnot a gzip stream",
    ];
    for data in inputs {
        let compressed = gzip(data);
        for mode in MODES {
            for bytes in [*data, compressed.as_slice()] {
                let output = stdin(mode, bytes);
                assert_skipped(&output, &["-"]);
                assert!(output.stdout.is_empty());
            }
        }
    }
}

#[test]
fn identifiable_corrupt_svg_and_svgz_still_fail() {
    let svg = b"<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"><g></svg>";
    let compressed = gzip(svg);
    let escaped_namespace = b"<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/sv&#x67;\" width=\"1\" height=\"1\"><g></svg>";
    let declared_namespace = b"<!DOCTYPE svg [<!-- [ > ' --><?probe [ ]?><!ENTITY ns 'http://www.w3.org/2000/svg'>]><!-- before root --><s:svg xmlns:s='&ns;' width='1' height='1' data-note='>'><g></s:svg>";
    let invalid_utf8 = [svg.as_slice(), b"\xff"].concat();
    let mut truncated =
        gzip(b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"/>");
    truncated.truncate(truncated.len() - 8);
    for mode in MODES {
        for data in [
            svg.as_slice(),
            &compressed,
            &truncated,
            escaped_namespace,
            declared_namespace,
            &invalid_utf8,
        ] {
            let output = stdin(mode, data);
            assert_eq!(output.status.code(), Some(1), "{:?}", output.stderr);
            assert!(!String::from_utf8_lossy(&output.stderr).contains("skipping"));
        }
    }
}

#[test]
fn non_image_stdin_is_skipped_in_every_mode() {
    let xml = b"<?xml version=\"1.0\"?><document>not an image</document>";
    let gzip = gzip(b"not an image");
    for mode in MODES {
        for data in [b"not an image".as_slice(), b"", xml, &gzip] {
            let output = stdin(mode, data);
            assert_skipped(&output, &["-"]);
            assert!(output.stdout.is_empty());
        }
    }
}

#[test]
fn svg_and_svgz_stdin_still_render() {
    let svg = b"<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"><rect width=\"1\" height=\"1\" fill=\"red\"/></svg>";
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(b"<!-- SVGZ with a leading comment -->")
        .unwrap();
    gzip.write_all(svg.strip_prefix(b"<?xml version=\"1.0\"?>").unwrap())
        .unwrap();
    let gzip = gzip.finish().unwrap();
    for mode in MODES {
        for data in [svg.as_slice(), &gzip] {
            let output = stdin(mode, data);
            assert!(output.status.success(), "{:?}", output.stderr);
            assert!(output.stderr.is_empty());
            assert!(!output.stdout.is_empty());
        }
    }
}

#[test]
fn image_detection_uses_contents_instead_of_extensions() {
    let dir = std::env::temp_dir().join(format!("isee-non-images-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let image = dir.join("image.txt");
    let text = dir.join("text.png");
    std::fs::write(&image, b"P6\n1 1\n255\n\xff\x00\x00").unwrap();
    std::fs::write(&text, b"not an image").unwrap();
    for mode in MODES {
        let baseline = command(mode).args([&image, &image]).output().unwrap();
        assert!(baseline.status.success(), "{:?}", baseline.stderr);
        let output = command(mode)
            .args([&text, &image, &image])
            .output()
            .unwrap();
        assert_skipped(&output, &[text.to_str().unwrap()]);
        assert_eq!(output.stdout, baseline.stdout, "mode: {mode:?}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unreadable_or_corrupt_images_still_fail() {
    for mode in MODES {
        let output = command(mode)
            .args(["tests", "tests/fixtures/missing-non-image.png", IMAGE])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(!output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("1 image failed"), "{stderr}");
        let output = stdin(mode, b"\x89PNG\r\n\x1a\n");
        assert_eq!(output.status.code(), Some(1));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("skipping"));
    }
}
