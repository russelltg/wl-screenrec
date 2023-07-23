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
#[ignore]
fn history_clip_length() {
    let filename = temp_dir().join("ahcl.mp4");

    let mut cmd = Command::new(dbg!(wl_screenrec()))
        .arg("--no-damage")
        .arg("--history")
        .arg("2")
        .arg("-f")
        .arg(&filename)
        .spawn()
        .unwrap();

    sleep(Duration::from_secs(5));

    let pid = Pid::from_raw(cmd.id() as i32);
    kill(pid, SIGUSR1).unwrap();

    sleep(Duration::from_secs(6));

    kill(pid, SIGINT).unwrap();

    let wait_start = Instant::now();
    cmd.wait().unwrap();
    assert!(wait_start.elapsed() < Duration::from_secs(1));

    let dur = file_duration(filename);

    // duration *should* be ~8 (2 seconds of history + 6 seconds after USER1)
    assert!(dur > Duration::from_secs(8), "{:?} < 8s", dur);
    assert!(dur < Duration::from_secs_f64(8.5), "{:?} > 8.5s", dur);
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

    let dur = file_duration(filename);

    assert!(dur > Duration::from_secs_f64(2.5), "{:?} < 2.5s", dur);
    assert!(dur < Duration::from_secs_f64(3.5), "{:?} > 3.5s", dur);
}

fn file_duration(filename: PathBuf) -> Duration {
    let json: Value = serde_json::from_str(
        &String::from_utf8(
            Command::new("ffprobe")
                .arg("-show_format")
                .arg("-print_format")
                .arg("json")
                .arg(&filename)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap(),
    )
    .unwrap();

    let dur: f64 = json
        .pointer("/format/duration")
        .unwrap()
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    Duration::from_secs_f64(dur)
}
