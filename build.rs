use std::env;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(ffmpeg_7_0)");
    println!("cargo::rustc-check-cfg=cfg(ffmpeg_8_0)");

    for (name, value) in env::vars() {
        if name.starts_with("DEP_FFMPEG_") && !value.is_empty() {
            println!(
                r#"cargo::rustc-cfg={}"#,
                name["DEP_FFMPEG_".len()..name.len()].to_lowercase()
            );
        }
    }
}
