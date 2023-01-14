# wl-screenrec

High performance wlroots based screen recorder. Uses dma-buf transfers to get surface,
and uses the GPU to do both the pixel format conversion and the encoding, making it about
as performant as you could hope. 

Tested with Intel GPUs, but it's possible it works on other GPUs too, so long they support vaapi. Open a PR
if there are issues or if you've tested in on AMD/Nvidia and you want to update this documentation!


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

