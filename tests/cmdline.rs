use std::{
    env::{consts::EXE_SUFFIX, current_exe, temp_dir},
    path::{Path, PathBuf},
    process::Command,
    thread::sleep,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{
        kill,
        Signal::{SIGINT, SIGUSR1},
    },
    unistd::Pid,
};
use serde_json::Value;

fn wl_screenrec() -> PathBuf {
    let mut cur = current_exe().unwrap();
    cur.pop();
    cur.pop();
    cur.push(format!("wl-screenrec{}", EXE_SUFFIX));
    cur
}

#[test]
fn history_clip_length() {
    let filename = temp_dir().join("ahcl.mp4");

    let mut cmd = Command::new(dbg!(wl_screenrec()))
        .arg("--no-damage")
        .arg("--audio")
        .arg("--gop-size=5")
        .arg("--history=2")
        .arg("-f")
        .arg(&filename)
        .spawn()
        .unwrap();

    sleep(Duration::from_secs(10));

    let pid = Pid::from_raw(cmd.id() as i32);
    kill(pid, SIGUSR1).unwrap();

    sleep(Duration::from_secs(6));

    kill(pid, SIGINT).unwrap();

    let wait_start = Instant::now();
    cmd.wait().unwrap();
    assert!(wait_start.elapsed() < Duration::from_secs(1));

    let dur = file_duration(&filename);
    println!("dur={dur:?}");

    // duration *should* be ~8 (2 seconds of history + 6 seconds after USER1)
    assert!(dur > Duration::from_secs(8), "{:?} < 8s", dur);
    assert!(dur < Duration::from_secs_f64(8.5), "{:?} > 8.5s", dur);
}

#[test]
fn scale() {
    let filename = temp_dir().join("scale.mp4");

    let mut cmd = Command::new(dbg!(wl_screenrec()))
        .arg("--no-damage")
        .arg("--encode-resolution=128x128")
        .arg("-g=0,0 256x256")
        .arg("-f")
        .arg(&filename)
        .spawn()
        .unwrap();

    sleep(Duration::from_secs(5));
    let pid = Pid::from_raw(cmd.id() as i32);
    kill(pid, SIGINT).unwrap();

    let wait_start = Instant::now();
    cmd.wait().unwrap();
    assert!(wait_start.elapsed() < Duration::from_secs(1));

    assert_eq!(file_resolution(&filename), (128, 128));
}

#[test]
fn basic() {
    let filename = temp_dir().join("basic.mp4");

    let mut cmd = Command::new(dbg!(wl_screenrec()))
        .arg("--no-damage")
        .arg("-f")
        .arg(&filename)
        .spawn()
        .unwrap();
    let pid = Pid::from_raw(cmd.id() as i32);

    sleep(Duration::from_secs(3));

    kill(pid, SIGINT).unwrap();

    let wait_start = Instant::now();
    cmd.wait().unwrap();
    assert!(wait_start.elapsed() < Duration::from_secs(1));

    let dur = file_duration(&filename);

    assert!(dur > Duration::from_secs_f64(2.5), "{:?} < 2.5s", dur);
    assert!(dur < Duration::from_secs_f64(3.5), "{:?} > 3.5s", dur);
}

fn file_metadata(filename: &Path) -> Value {
    serde_json::from_str(
        &String::from_utf8(
            Command::new("ffprobe")
                .arg("-show_format")
                .arg("-show_streams")
                .arg("-print_format")
                .arg("json")
                .arg(&filename)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap(),
    )
    .unwrap()
}

fn file_duration(filename: &Path) -> Duration {
    let json = file_metadata(filename);

    let dur: f64 = json
        .pointer("/format/duration")
        .unwrap()
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    Duration::from_secs_f64(dur)
}

fn file_resolution(filename: &Path) -> (i64, i64) {
    let json = file_metadata(filename);

    (
        json.pointer("/streams/0/width")
            .unwrap()
            .as_number()
            .unwrap()
            .as_i64()
            .unwrap(),
        json.pointer("/streams/0/height")
            .unwrap()
            .as_number()
            .unwrap()
            .as_i64()
            .unwrap(),
    )
}
