# wl-screenrec

High performance wlroots based screen recorder. Uses dma-buf transfers to get surface,
and uses the GPU to do both the pixel format conversion and the encoding, making it about
as performant as you could hope. 

Tested with Intel GPUs, but it's possible it works on other GPUs too, so long they support vaapi. Open a PR
if there are issues or if you've tested in on AMD/Nvidia and you want to update this documentation!

# Performance

(relatively unscientific) benchmark setup:
- 4kp60 display
- i9-11900H CPU/GPU
- `vkcube` running on screen, as both `wf-recorder` and `wl-screenrec` don't copy/encode frames when there is no difference

| command                                                   | steady state CPU usage by recording app |
| --------------------------------------------------------- | --------------------------------------- |
| `wf-recorder`                                             | ~500%                                   |
| `wf-recorder --codec h264_vaapi --device  /dev/dri/card0` | ~75%                                    |
| `wl-screenrec`                                            | ~2.5%                                   |

# Installation

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
- Has some at-exit memory leaks. I'll eventually figure them out probably. I'm not super familar with the ffmpeg api, and the ffmpeg-next isn't complete enough for this project, so it uses a combination of ffmpeg-next and the C ffi.
- For some reason mp4 output seems to be broken. AVI works great. I have no idea why!
