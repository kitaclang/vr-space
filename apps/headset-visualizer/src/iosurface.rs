#![cfg(target_os = "macos")]

//! Let the GPU read captured frames in place instead of copying them.
//!
//! ScreenCaptureKit delivers each frame in an IOSurface: memory the GPU can read directly.
//! `import` wraps one in a Metal texture and hands it to wgpu as a normal `wgpu::Texture`, so
//! showing a frame costs no CPU copy and no upload. ScreenCaptureKit cycles through a small
//! pool of surfaces, so `Scene` keeps the imported textures by `id` and, once each surface
//! has been seen, a new frame only switches which bind group is drawn.
//!
//! A texture made from a surface doesn't stop ScreenCaptureKit from writing the next frame
//! into it: whoever shows the frame must also keep its pixel buffer alive (see
//! `CapturedFrame`) until the GPU has finished drawing it.

use std::ffi::c_void;

use metal::foreign_types::ForeignType;
use objc2::encode::{Encoding, RefEncode};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;

/// An IOSurface, borrowed: the caller keeps it alive.
#[repr(C)]
pub struct IOSurface {
    _private: [u8; 0],
}

// Metal's method signatures name the type, and objc2 checks them in debug builds.
unsafe impl RefEncode for IOSurface {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__IOSurface", &[]));
}

pub type IOSurfaceRef = *mut IOSurface;

/// `'BGRA'`, ScreenCaptureKit's `PixelFormat::BGRA`.
const PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    fn IOSurfaceGetID(surface: IOSurfaceRef) -> u32;
    fn IOSurfaceGetWidth(surface: IOSurfaceRef) -> usize;
    fn IOSurfaceGetHeight(surface: IOSurfaceRef) -> usize;
    fn IOSurfaceGetPixelFormat(surface: IOSurfaceRef) -> u32;
    fn IOSurfaceGetPlaneCount(surface: IOSurfaceRef) -> usize;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferGetIOSurface(pixel_buffer: *const c_void) -> IOSurfaceRef;
}

/// The IOSurface behind a CVPixelBuffer, if it has one. It lives as long as the buffer.
///
/// # Safety
/// `pixel_buffer` must be a live CVPixelBufferRef.
pub unsafe fn surface_of_pixel_buffer(pixel_buffer: *const c_void) -> Option<IOSurfaceRef> {
    let surface = unsafe { CVPixelBufferGetIOSurface(pixel_buffer) };
    (!surface.is_null()).then_some(surface)
}

/// A number that identifies the surface while it exists.
///
/// # Safety
/// `surface` must be a live IOSurface.
pub unsafe fn id(surface: IOSurfaceRef) -> u32 {
    unsafe { IOSurfaceGetID(surface) }
}

/// Whether `device` runs on Metal, which `import` needs.
pub fn device_supports_import(device: &wgpu::Device) -> bool {
    // SAFETY: only checks which backend the device uses.
    unsafe { device.as_hal::<wgpu::hal::api::Metal, _, _>(|d| d.is_some()) }
}

/// Wrap a BGRA surface of `width` x `height` pixels as a texture the screen quad can sample.
/// `None` when the device isn't Metal or the surface isn't a single-plane BGRA surface of
/// that size. The texture keeps the surface's memory alive; it doesn't copy it.
///
/// # Safety
/// `surface` must be a live IOSurface.
pub unsafe fn import(device: &wgpu::Device, surface: IOSurfaceRef, width: u32, height: u32) -> Option<wgpu::Texture> {
    // SAFETY: the caller guarantees `surface` is live.
    let matches = unsafe {
        IOSurfaceGetPixelFormat(surface) == PIXEL_FORMAT_BGRA
            && IOSurfaceGetPlaneCount(surface) == 0
            && IOSurfaceGetWidth(surface) == width as usize
            && IOSurfaceGetHeight(surface) == height as usize
    };
    if !matches || width == 0 || height == 0 {
        return None;
    }

    // SAFETY: the Metal device is only used inside the callback, to create the texture.
    let raw = unsafe {
        device.as_hal::<wgpu::hal::api::Metal, _, _>(|hal| {
            let hal = hal?;
            let mtl = hal.raw_device().lock();
            let desc = metal::TextureDescriptor::new();
            desc.set_texture_type(metal::MTLTextureType::D2);
            desc.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
            desc.set_width(width as u64);
            desc.set_height(height as u64);
            desc.set_mipmap_level_count(1);
            desc.set_usage(metal::MTLTextureUsage::ShaderRead);
            let device_obj = &*(mtl.as_ptr() as *const AnyObject);
            let desc_obj = &*(desc.as_ptr() as *const AnyObject);
            let texture: Option<Retained<AnyObject>> =
                objc2::msg_send![device_obj, newTextureWithDescriptor: desc_obj, iosurface: surface, plane: 0usize];
            // Hand the +1 reference from `new...` to metal-rs.
            texture.map(|t| metal::Texture::from_ptr(Retained::into_raw(t) as *mut metal::MTLTexture))
        })
    }?;

    let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
    // SAFETY: `raw` is a 2D BGRA8Unorm texture of `size` with one mip level and one layer,
    // created on this device, matching both descriptors. wgpu treats it as initialised, so
    // it never clears it.
    let texture = unsafe {
        let hal_texture = wgpu::hal::metal::Device::texture_from_raw(
            raw,
            wgpu::TextureFormat::Bgra8Unorm,
            metal::MTLTextureType::D2,
            1,
            1,
            wgpu::hal::CopyExtent { width, height, depth: 1 },
        );
        device.create_texture_from_hal::<wgpu::hal::api::Metal>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("capture-iosurface"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
        )
    };
    Some(texture)
}

#[cfg(test)]
pub mod test_surface {
    //! IOSurfaces made in tests, standing in for the ones ScreenCaptureKit delivers.

    use super::*;
    use core_foundation::base::{CFRelease, CFType, TCFType};
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;

    #[link(name = "IOSurface", kind = "framework")]
    extern "C" {
        fn IOSurfaceCreate(properties: CFDictionaryRef) -> IOSurfaceRef;
        fn IOSurfaceLock(surface: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceUnlock(surface: IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
        fn IOSurfaceGetBaseAddress(surface: IOSurfaceRef) -> *mut c_void;
        fn IOSurfaceGetBytesPerRow(surface: IOSurfaceRef) -> usize;
    }

    /// An owned BGRA IOSurface.
    pub struct TestSurface {
        pub surface: IOSurfaceRef,
        pub width: u32,
        pub height: u32,
    }

    impl TestSurface {
        pub fn new(width: u32, height: u32, pixel_format: u32) -> Self {
            let pair = |k: &str, v: i64| (CFString::new(k).as_CFType(), CFNumber::from(v).as_CFType());
            let props: CFDictionary<CFType, CFType> = CFDictionary::from_CFType_pairs(&[
                pair("IOSurfaceWidth", width.into()),
                pair("IOSurfaceHeight", height.into()),
                pair("IOSurfaceBytesPerElement", 4),
                pair("IOSurfacePixelFormat", pixel_format.into()),
            ]);
            // SAFETY: a valid properties dictionary; the +1 surface is released in Drop.
            let surface = unsafe { IOSurfaceCreate(props.as_concrete_TypeRef()) };
            assert!(!surface.is_null(), "IOSurfaceCreate failed");
            Self { surface, width, height }
        }

        pub fn bgra(width: u32, height: u32) -> Self {
            Self::new(width, height, PIXEL_FORMAT_BGRA)
        }

        /// Fill every pixel with `color` (BGRA), as the capture would by writing a frame.
        pub fn fill(&self, color: [u8; 4]) {
            // SAFETY: the surface is locked while its memory is written, row by row within
            // its stride.
            unsafe {
                assert_eq!(IOSurfaceLock(self.surface, 0, std::ptr::null_mut()), 0);
                let base = IOSurfaceGetBaseAddress(self.surface) as *mut u8;
                let stride = IOSurfaceGetBytesPerRow(self.surface);
                for y in 0..self.height as usize {
                    let row = std::slice::from_raw_parts_mut(base.add(y * stride), self.width as usize * 4);
                    for px in row.chunks_exact_mut(4) {
                        px.copy_from_slice(&color);
                    }
                }
                assert_eq!(IOSurfaceUnlock(self.surface, 0, std::ptr::null_mut()), 0);
            }
        }

        pub fn id(&self) -> u32 {
            // SAFETY: the surface is live while `self` is.
            unsafe { id(self.surface) }
        }
    }

    impl Drop for TestSurface {
        fn drop(&mut self) {
            // SAFETY: releases the reference from IOSurfaceCreate.
            unsafe { CFRelease(self.surface as *const c_void) };
        }
    }
}
