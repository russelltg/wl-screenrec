# wl-screenrec

High performance wlroots based screen recorder. Uses dma-buf transfers to get surface,
and uses the GPU to do both the pixel format conversion and the encoding, making it about
as performant as you could hope. 

Tested with Intel GPUs, but it's possible it works on other GPUs too, so long they support vaapi. Open a PR
or issue if there are problems or if you've tested in on AMD/Nvidia and you want to update this documentation!

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

# Known issues

- Cannot capture a region that spans more than one display. This is probably possible but quite difficult, espeicially with potential differences in refresh rate. Probably will never be supported.
- For some reason mp4 output seems to be broken. AVI works great. I have no idea why!
