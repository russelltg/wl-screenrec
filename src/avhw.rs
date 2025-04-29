use std::{ffi::CString, path::Path, ptr::null_mut};

use ffmpeg::{
    dict,
    ffi::{
        av_buffer_ref, av_buffer_unref, av_hwdevice_ctx_create, av_hwdevice_ctx_create_derived,
        av_hwframe_ctx_alloc, av_hwframe_ctx_init, av_hwframe_get_buffer, AVHWFramesContext,
    },
    format::Pixel,
    frame,
};

use crate::DrmModifier;
use log::error;

pub struct AvHwDevCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,
    fmt: Pixel,
}

impl AvHwDevCtx {
    pub fn new_libva(dri_device: &Path) -> Result<Self, ffmpeg::Error> {
        unsafe {
            let mut hw_device_ctx = null_mut();

            let opts = dict! {
                "connection_type" => "drm"
            };

            let dev_cstr = CString::new(dri_device.to_str().unwrap()).unwrap();
            let sts = av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                dev_cstr.as_ptr(),
                opts.as_mut_ptr(),
                0,
            );

            if sts != 0 {
                Err(ffmpeg::Error::from(sts))
            } else {
                Ok(Self {
                    ptr: hw_device_ctx,
                    fmt: Pixel::VAAPI,
                })
            }
        }
    }

    pub fn new_vulkan(dri_device: &Path) -> Result<Self, ffmpeg::Error> {
        unsafe {
            let mut hw_device_ctx_drm = null_mut();
            let mut hw_device_ctx = null_mut();

            let dev_cstr = CString::new(dri_device.to_str().unwrap()).unwrap();

            let sts = av_hwdevice_ctx_create(
                &mut hw_device_ctx_drm,
                ffmpeg_sys_next::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                dev_cstr.as_ptr(),
                null_mut(),
                0,
            );
            if sts != 0 {
                return Err(ffmpeg::Error::from(sts));
            }

            let sts = av_hwdevice_ctx_create_derived(
                &mut hw_device_ctx,
                ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
                hw_device_ctx_drm,
                0,
            );

            av_buffer_unref(&mut hw_device_ctx_drm);

            if sts != 0 {
                Err(ffmpeg::Error::from(sts))
            } else {
                Ok(Self {
                    ptr: hw_device_ctx,
                    fmt: Pixel::VULKAN,
                })
            }
        }
    }

    pub fn create_frame_ctx(
        &mut self,
        pixfmt: Pixel,
        width: i32,
        height: i32,
        modifiers: &[DrmModifier],
    ) -> Result<AvHwFrameCtx, ffmpeg::Error> {
        unsafe {
            let mut hwframe = av_hwframe_ctx_alloc(self.ptr as *mut _);
            let hwframe_casted = &mut *((*hwframe).data as *mut AVHWFramesContext);

            // ffmpeg does not expose RGB vaapi
            hwframe_casted.format = self.fmt.into();
            hwframe_casted.sw_format = pixfmt.into();
            hwframe_casted.width = width;
            hwframe_casted.height = height;
            hwframe_casted.initial_pool_size = 5;

            let mut sts = -1;

            #[cfg(feature = "experimental-vulkan")]
            let mut drm_info = None;

            #[cfg(feature = "experimental-vulkan")]
            let mut vk_modifiers = None;

            
            if self.fmt == Pixel::VULKAN {
                #[cfg(feature = "experimental-vulkan")]
                {
                    use ash::vk;
                    use ffmpeg::ffi::{
                        av_vkfmt_from_pixfmt, AVHWDeviceContext, AVVulkanDeviceContext,
                        AVVulkanFramesContext,
                    };

                    let av_devctx = &(*((*self.as_mut_ptr()).data as *mut AVHWDeviceContext));
                    let vk_hwctx = &*(av_devctx.hwctx as *mut AVVulkanDeviceContext);

                    let inst = ash::Instance::load(
                        &ash::StaticFn {
                            get_instance_proc_addr: vk_hwctx.get_proc_addr,
                        },
                        vk_hwctx.inst,
                    );

                    let mut modifiers_filtered: Vec<DrmModifier> = Vec::new();
                    for modifier in modifiers {
                        let mut drm_info = ash::vk::PhysicalDeviceImageDrmFormatModifierInfoEXT {
                            drm_format_modifier: modifier.0,

                            ..Default::default()
                        };
                        let mut image_format_prop = ash::vk::ImageFormatProperties2 {
                            ..Default::default()
                        };

                        if let Ok(()) = inst.get_physical_device_image_format_properties2(
                            vk_hwctx.phys_dev,
                            &vk::PhysicalDeviceImageFormatInfo2 {
                                format: *av_vkfmt_from_pixfmt(pixfmt.into()),
                                p_next: &mut drm_info as *mut _ as _,
                                ..Default::default()
                            },
                            &mut image_format_prop,
                        ) {
                            modifiers_filtered.push(*modifier);
                        }
                    }

                    drm_info = Some(Box::new(ash::vk::ImageDrmFormatModifierListCreateInfoEXT::default()));
                    let drm_info = drm_info.as_deref_mut().unwrap();

                    // some buffer requirements are complex, just start removing modifiers until it works
                    while sts != 0 && !modifiers_filtered.is_empty() {
                        drm_info.drm_format_modifier_count = modifiers_filtered.len() as u32;
                        drm_info.p_drm_format_modifiers = modifiers_filtered.as_ptr() as _;

                        let vk_ptr = hwframe_casted.hwctx as *mut AVVulkanFramesContext;

                        (*vk_ptr).tiling = vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT;
                        (*vk_ptr).create_pnext = drm_info as *mut ash::vk::ImageDrmFormatModifierListCreateInfoEXT as _;

                        sts = av_hwframe_ctx_init(hwframe);
                        
                        if sts != 0 {
                            modifiers_filtered.pop();
                        }

                    }

                    vk_modifiers = Some(modifiers_filtered.into_boxed_slice());
                }
                #[cfg(not(feature = "experimental-vulkan"))]
                panic!("vulkan requested but built without vulkan support")
            } else {
                if modifiers != &[DrmModifier::LINEAR] {
                    error!("unknown how to request non-linear frames in vaapi");
                }
                sts = av_hwframe_ctx_init(hwframe);
            }
            if sts != 0 {
                return Err(ffmpeg::Error::from(sts));
            }

            let ret = Ok(AvHwFrameCtx {
                ptr: av_buffer_ref(hwframe),

                #[cfg(feature = "experimental-vulkan")]
                _drm_info: drm_info,
                #[cfg(feature = "experimental-vulkan")]
                _vk_modifiers: vk_modifiers,
            });

            av_buffer_unref(&mut hwframe);

            ret
        }
    }

    pub fn as_mut_ptr(&mut self) -> *mut ffmpeg::sys::AVBufferRef {
        self.ptr
    }
}

impl Drop for AvHwDevCtx {
    fn drop(&mut self) {
        unsafe {
            av_buffer_unref(&mut self.ptr);
        }
    }
}

pub struct AvHwFrameCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,

    // the frame context continues to references these pointeres, so allocate them on the heap
    #[cfg(feature = "experimental-vulkan")]
    _drm_info: Option<Box<ash::vk::ImageDrmFormatModifierListCreateInfoEXT<'static>>>, // static is a hack, it's really the lifetime of vk_modifiers

    #[cfg(feature = "experimental-vulkan")]
    _vk_modifiers: Option<Box<[DrmModifier]>>,
}

impl Drop for AvHwFrameCtx {
    fn drop(&mut self) {
        unsafe {
            av_buffer_unref(&mut self.ptr);
        }
    }
}

impl AvHwFrameCtx {
    pub fn alloc(&mut self) -> Result<frame::Video, ffmpeg::Error> {
        let mut frame = ffmpeg_next::frame::video::Video::empty();
        match unsafe { av_hwframe_get_buffer(self.ptr, frame.as_mut_ptr(), 0) } {
            0 => Ok(frame),
            e => Err(ffmpeg::Error::from(e)),
        }
    }
    pub fn as_mut_ptr(&mut self) -> *mut ffmpeg::sys::AVBufferRef {
        self.ptr
    }
}
