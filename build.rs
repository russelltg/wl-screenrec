fn main() {
    // for drmGetRenderDeviceNameFromFd
    println!("cargo:rustc-link-lib=drm");
}