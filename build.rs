use std::env;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(ffmpeg_7_0)");
    for (name, _value) in env::vars() {
        if name.starts_with("DEP_FFMPEG_") {
            println!(
                r#"cargo::rustc-cfg={}"#,
                name["DEP_FFMPEG_".len()..name.len()].to_lowercase()
            );
        }
    }
}
