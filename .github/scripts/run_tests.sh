#!/bin/bash

set -x

code=0

rustup install nightly
if [ $? -ne 0 ] ; then code=$? ; fi

git clone https://github.com/russelltg/libva-x264
if [ $? -ne 0 ] ; then code=$? ; fi

(cd libva-x264 && cargo +nightly build --release)
if [ $? -ne 0 ] ; then code=$? ; fi

cp libva-x264/target/release/liblibva_x264.so x264_drv_video.so
if [ $? -ne 0 ] ; then code=$? ; fi

LIBVA_DRIVERS_PATH=`pwd` LIBVA_DRIVER_NAME=x264 cargo test
if [ $? -ne 0 ] ; then code=$? ; fi

swaymsg exit

echo $code > /tmp/test_exit_code

