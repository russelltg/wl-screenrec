# wl-screenrec

[![crates.io](https://img.shields.io/crates/v/wl-screenrec.svg)](https://crates.io/crates/wl-screenrec)

High performance screen recorder for wlroots Wayland.

Uses dma-buf transfers to get surface, and uses the GPU to do both the pixel format conversion and the encoding,
meaning the raw video data never touches the CPU, leaving it free to run your applications.

Open an issue if something is not working, I'm happy to take a look.

# System Requirements

* wayland compositor supporting the following protocols:
  * [`wlr-screencopy-unstable-v1`](https://wayland.app/protocols/wlr-screencopy-unstable-v1)
  * [`linux-dmabuf-v1`](https://wayland.app/protocols/linux-dmabuf-v1)
  * [`xdg-output-unstable-v1`](https://wayland.app/protocols/xdg-output-unstable-v1)

   [Sway](https://swaywm.org/), [Hyprland](https://hyprland.org/), and [Wayfire](https://wayfire.org/) all meet this criteria.
* [`vaapi`](https://01.org/temp-linuxgraphics/community/vaapi) encode support, consult your distribution for how to set this up. Known good configurations:
  * Intel iGPUs
  * Radeon GPUs[^1]

[^1]: Mesa vaapi (which Radeon GPUs use) does not support transform, so `wl-screenrec` will not work if you have a monitor that has transform applied to it

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
paru -S wl-screenrec
# OR
paru -S wl-screenrec-git
```
Or, manually:
```bash
git clone https://aur.archlinux.org/wl-screenrec-git.git
cd wl-screenrec-git
makepkg -si
```

## From source using cargo

Install ffmpeg 6 or later, which is a required dependency.
ffmpeg 5 may work, but is untested (open an issue or PR if you test with ffmpeg 5
so I can update these docs on if it works or not)

```bash
cargo install wl-screenrec # stable version
# OR
cargo install --git https://github.com/russelltg/wl-screenrec # git version
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
wl-screenrec -o DP-1 # specify output
```

Capture region:

```bash
wl-screenrec -g "$(slurp)"    # use slurp
wl-screenrec -g "0,0 128x128" # manual region
```

Capture 444 video (no pixel format compression):

> NOTE: Look at `vainfo -a` to see your supported pixel formats. Support is very
> hardware-dependent. For example, on my machine only HEVC suports 444 formats, and
> all of 8-bit RGB formats didn't work for whatever reason.

```bash
wl-screenrec --codec hevc --encode-pixfmt vuyx   # 8-bit 444
wl-screenrec --codec hevc --encode-pixfmt xrgb10 # 10-bit 444
```

Capture with audio:

```bash
wl-screenrec --audio                                                                 # default capture device
wl-screenrec --audio --audio-device alsa_output.pci-0000_00_1f.3.hdmi-stereo.monitor # capture desktop audio (example, use `pactl list short sources` to figure out what you should put here)
```

Record with history:
```bash
wl-screenrec --history 10 & # record the most recent 10 seconds into memory, not writing into the file
# ... some important event occurs
killall -USR1 wl-screenrec  # flush the most recent 10 seconds onto the file, and start appending to the file like recording normally
```

Capture to [v4l2loopback](https://github.com/umlaeute/v4l2loopback) (for Zoom, etc):

```bash
sudo modprobe v4l2loopback
v4l2-ctl --list-devices # find "Dummy video device" device. /dev/video6 in my case.
wl-screenrec --ffmpeg-muxer v4l2 -f /dev/video6
```

# All options

```text
$ wl-screenrec --help
High performance screen/audio recorder for wlroots

Usage: wl-screenrec [OPTIONS]

Options:
      --no-hw
          don't use the GPU encoder, download the frames onto the CPU and use a software encoder. Ignored if `encoder` is supplied
  -f, --filename <FILENAME>
          filename to write to. container type is detected from extension [default: screenrecord.mp4]
  -g, --geometry <GEOMETRY>
          geometry to capture, format x,y WxH. Compatible with the output of `slurp`. Mutually exclusive with --output
  -o, --output <OUTPUT>
          Which output (display) to record. Mutually exclusive with --geometry. Defaults to your only display if you only have one [default: ]
  -v, --verbose...
          add very loud logging. can be specified multiple times
      --dri-device <DRI_DEVICE>
          which dri device to use for vaapi. by default, this is obtained from the linux-dmabuf-v1 protocol when using wlr-screencopy, and from ext-image-copy-capture-session if using ext-image-copy-capture, if present. if not present, /dev/dri/renderD128 is guessed
      --low-power <LOW_POWER>
          [default: auto] [possible values: auto, on, off]
      --codec <CODEC>
          which video codec to use. Ignored if `--ffmpeg-encoder` is supplied [default: auto] [possible values: auto, avc, hevc, vp8, vp9, av1]
      --ffmpeg-muxer <FFMPEG_MUXER>
          Which ffmpeg muxer to use. Guessed from output filename by default
      --ffmpeg-muxer-options <FFMPEG_MUXER_OPTIONS>
          Options to pass to the muxer. Format looks like key=val,key2=val2
      --ffmpeg-encoder <FFMPEG_ENCODER>
          Use this to force a particular ffmpeg encoder. Generally, this is not necessary and the combo of --codec and --hw can get you to where you need to be
      --ffmpeg-encoder-options <FFMPEG_ENCODER_OPTIONS>
          Options to pass to the encoder. Format looks like key=val,key2=val2
      --audio-codec <AUDIO_CODEC>
          Which audio codec to use. Ignored if `--ffmpeg-audio-encoder` is supplied [default: auto] [possible values: auto, aac, mp3, flac, opus]
      --audio-bitrate <AUDIO_BITRATE>
          audio bitrate to encode at. Unit is bytes per second, 16 KB is 128 kbps [default: "16 kB"]
      --ffmpeg-audio-encoder <FFMPEG_AUDIO_ENCODER>
          Use this to force a particular audio ffmpeg encoder. By default, this is guessed from the muxer (which is guess by the file extension if --ffmpeg-muxer isn't passed)
      --encode-pixfmt <ENCODE_PIXFMT>
          which pixel format to encode with. not all codecs will support all pixel formats. This should be a ffmpeg pixel format string, like nv12 or x2rgb10. If the encoder supports vaapi memory, it will use this pixel format type but in vaapi memory
      --encode-resolution <ENCODE_RESOLUTION>
          what resolution to encode at. example: 1920x1080. Default is the resolution of the captured region. If your goal is reducing filesize, it's suggested to try --bitrate/-b first
  -b, --bitrate <BITRATE>
          bitrate to encode at. Unit is bytes per second, so 5 MB is 40 Mbps [default: "5 MB"]
      --history <HISTORY>
          run in a mode where the screen is recorded, but nothing is written to the output file until SIGUSR1 is sent to the process. Then, it writes the most recent N seconds to a file and continues recording
      --audio
          record audio with the stream. Defaults to the default audio capture device
      --audio-device <AUDIO_DEVICE>
          which audio device to record from. list devices with `pactl list short sources` [default: default]
      --audio-backend <AUDIO_BACKEND>
          which ffmpeg audio capture backend (see https://ffmpeg.org/ffmpeg-devices.html`) to use. you almost certainally want to specify --audio-device if you use this, as the values depend on the backend used [default: pulse]
      --no-damage
          copy every frame, not just unique frames. This can be helpful to get a non-variable framerate video, but is generally discouraged as it uses much more resources. Useful for testing
      --gop-size <GOP_SIZE>
          GOP (group of pictures) size
      --generate-completions <COMPLETIONS_GENERATOR>
          print completions for the specified shell to stdout [possible values: bash, elvish, fish, powershell, zsh]
      --experimental-ext-image-copy-capture
          use the new ext-image-copy-capture protocol
  -h, --help
          Print help
  -V, --version
          Print version
```

# Known issues

- Cannot capture a region that spans more than one display. This is probably possible but quite difficult, espeicially with potential differences in refresh rate. Probably will never be supported.
