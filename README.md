# wl-screenrec

High performance screen recorder for wlroots Wayland. 

Uses dma-buf transfers to get surface, and uses the GPU to do both the pixel format conversion and the encoding,
meaning the raw video data never touches the CPU, leaving it free to run your applications.

Open an issue if something is not working, I'm happy to take a look.

# System Requirements

* wayland compositor supporting the following unstable protocols:
  * [`wlr-output-management-unstable-v1`](https://wayland.app/protocols/wlr-output-management-unstable-v1) 
  * [`wlr-screencopy-unstable-v1`](https://wayland.app/protocols/wlr-screencopy-unstable-v1), 

   [Sway](https://swaywm.org/), [Hyprland](https://hyprland.org/), and [wayfire](https://wayfire.org/) all meet this criteria.
* [`vaapi`](https://01.org/temp-linuxgraphics/community/vaapi) encode support, consult your distribution for how to set this up. Known good configurations:
  * Intel iGPUs
  * Radeon GPUs

# Performance

(relatively unscientific) benchmark setup:
- 4kp60 display
- i9-11900H CPU/GPU
- `vkcube` running on screen, as both `wf-recorder` and `wl-screenrec` don't copy/encode frames when there is no difference

| command                                       | CPU Usage | GPU 3D Δ | GPU Video Δ |
| --------------------------------------------- | --------- | -------- | ----------- |
| `wf-recorder`                                 | ~500%     | +44%     | 0%          |
| `wf-recorder -c h264_vaapi -d /dev/dri/card0` | ~75%      | +88%     | +23%        |
| `wl-screenrec`                                | ~2.5%     | +91%     | +30%        |

Additionally, with either `wf-recorder` setup there is visible stuttering in the `vkcube` window. `wl-screenrec` does not seem to stutter at all.

However, it does come at the cost of using slightly more GPU. Those number seem stable and I hypothesize that they are statistically significant,
but still not a huge change.

# Installation

## From the AUR

```bash
paru -S wl-screenrec-git
```
Or, manually:
```
git clone https://aur.archlinux.org/wl-screenrec-git.git
cd wl-screenrec-git
makepkg -si
```

## From source using cargo

Install ffmpeg, which is a required dependency.

```bash
cargo install --git https://github.com/russelltg/wl-screenrec
```

## FreeBSD

There is currently an [upstream bug](https://github.com/zmwangx/rust-ffmpeg/pull/152) preventing
builds on FreeBSD from succeeding, but you can fix this by patching the `rust-ffmpeg` dependency:

```bash
git clone https://github.com/russelltg/wl-screenrec
cd wl-screenrec
echo '[patch.crates-io]
ffmpeg-next = { git = "https://github.com/russelltg/rust-ffmpeg", branch = "fix_freebsd_build" }' >> Cargo.toml
cargo install --path .
```

# Usage

Capture entire output:

```bash
wl-screenrec         # valid when you only have one output
wl-screenrec -o DP-1 # specify outuput
```

Capture region:

```bash
wl-screenrec -g "$(slurp)"    # use slurp
wl-screenrec -g "0,0 128x128" # manual region
```

Record with history
```bash
wl-screenrec --history 10 & # record the most recent 10 seconds into memory, not writing into the file
# ... some important event occurs
killall -USR1 wl-screenrec  # flush the most recent 10 seconds onto the file, and start appending to the file like recording normally
```

# Known issues

- Cannot capture a region that spans more than one display. This is probably possible but quite difficult, espeicially with potential differences in refresh rate. Probably will never be supported.
