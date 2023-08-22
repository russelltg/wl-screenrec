#!/bin/bash

git clone https://github.com/russelltg/libva-x264
(cd libva-x264 && cargo build --release)
cp libva-x264/target/release/liblibva_x264.so x264_drv_video.so

LIBVA_DRIVERS_PATH=`pwd` LIBVA_DRIVER_NAME=x264 cargo test

swaymsg exit
