use std::io::Write;
use std::process::{Command, Output, Stdio};

const IMAGE: &str = "tests/fixtures/anim.webp";

fn isee(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_isee"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(args)
        .env_remove("TMUX")
        .env("TERM_PROGRAM", "")
        .env("ISEE_KGP_TRANSFER", "stream")
        .output()
        .unwrap()
}

fn halfblock_cells(line: &str) -> Vec<&str> {
    line.strip_suffix("\x1b[0m")
        .unwrap()
        .split_inclusive(['▀', ' '])
        .collect()
}

fn wide_ppm() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("isee-wide-tile-{}.ppm", std::process::id()));
    let mut ppm = b"P6\n800 100\n255\n".to_vec();
    ppm.resize(ppm.len() + 800 * 100 * 3, 0xff);
    std::fs::write(&path, ppm).unwrap();
    path
}

#[test]
fn tile_rows_keep_order_and_the_incomplete_last_row() {
    let output = isee(&["-p", "halfblock", "-t3", IMAGE, IMAGE, IMAGE, IMAGE]);
    assert!(output.status.success(), "{:?}", output.stderr);
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    // The next title follows the last image line directly.
    assert_eq!(lines.len(), 22);
    assert_eq!(lines[0], format!("{IMAGE:<26} {IMAGE:<26} {IMAGE:<26}"));
    assert_eq!(lines[11], format!("{IMAGE:<26}"));
    for index in [1, 10, 12, 21] {
        let cells = halfblock_cells(lines[index]);
        assert_eq!(cells.len(), 80);
        assert_eq!(cells[26], "\x1b[0m ");
        assert_eq!(cells[53], "\x1b[0m ");
        if index >= 12 {
            assert!(cells[26..].iter().all(|cell| *cell == "\x1b[0m "));
        }
    }
}

#[test]
fn wide_rows_end_after_their_tallest_image() {
    let path = wide_ppm();
    let path = path.to_str().unwrap();
    let output = isee(&["-p", "halfblock", "-t3", path, path, path, path]);
    std::fs::remove_file(path).unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);

    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = text.lines().collect();
    // An 8:1 image uses two 14px character rows in the 182px-wide tiles.
    assert_eq!(lines.len(), 6);
    assert!(
        !lines[3].is_empty(),
        "the second title must follow the image"
    );
}

#[test]
fn ignored_width_and_animation_only_add_a_warning() {
    let baseline = isee(&["-p", "halfblock", "-t3", IMAGE]);
    for args in [
        vec!["-p", "halfblock", "-t3", "-w800", "-a", IMAGE],
        vec!["-a", "-w0", "-p", "halfblock", "-t3", IMAGE],
    ] {
        let output = isee(&args);
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(output.stdout, baseline.stdout);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr.lines().count(), 1);
        assert!(stderr.contains("-w and -a are ignored"));
    }
}

#[test]
fn failed_image_keeps_its_slot_and_later_images_still_render() {
    let missing = "tests/fixtures/missing-tile.png";
    let output = isee(&["-p", "halfblock", "-t3", IMAGE, missing, IMAGE]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with(&format!(
        "{IMAGE:<26} tests/fixtures/missing-ti… {IMAGE:<26}\r\n"
    )));
    // The first and third slots contain the same image, the middle one is blank.
    let lines: Vec<_> = text.lines().collect();
    for line in &lines[1..] {
        let cells = halfblock_cells(line);
        assert_eq!(cells.len(), 80);
        assert_eq!(&cells[..26], &cells[54..]);
        assert!(cells[26..54].iter().all(|cell| *cell == "\x1b[0m "));
    }
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(missing));
    assert!(stderr.contains("1 image failed"));
}

#[test]
fn info_takes_precedence_and_tile_counts_are_validated() {
    let baseline = isee(&["-i", IMAGE]);
    let output = isee(&["-i", "-t3", "-w800", "-a", IMAGE]);
    assert!(output.status.success());
    assert_eq!(output.stdout, baseline.stdout);
    assert!(output.stderr.is_empty());
    for args in [vec!["-t0", IMAGE], vec!["--tile", "3", IMAGE]] {
        assert_eq!(isee(&args).status.code(), Some(2));
    }
}

#[test]
fn stdin_has_a_title_in_tile_mode() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_isee"))
        .args(["-p", "halfblock", "-t3"])
        .env_remove("TMUX")
        .env("TERM_PROGRAM", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"P6\n1 1\n255\n\xff\x00\x00")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(
        output
            .stdout
            .starts_with(format!("{:<26}\r\n", "-").as_bytes())
    );
}
