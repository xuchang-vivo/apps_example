// Copyright (c) 2026 vivo Mobile Communication Co., Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! `/dev/fb0` output for the Slint software renderer (spec §6): open,
//! validate, and draw line-by-line so the app never allocates a full-frame
//! RGB565 buffer (~300 KB). The framebuffer device only supports
//! read/write/ioctl — no mmap.

use librs::c_str::CStr;
use librs::syscall::Syscall;
use slint::platform::software_renderer::{LineBufferProvider, Rgb565Pixel};
use std::io::{Error, ErrorKind, Result as IoResult};

/// Native ST7796 panel resolution.
pub(super) const LCD_H_RES: u16 = 320;
pub(super) const LCD_V_RES: u16 = 480;

/// ~12 fps. Full-screen repaint is bounded by the /dev/fb0 write rate on
/// the SPI bus shared with the MAX7219 and the flash, not by CPU (spec §6).
pub(super) const FRAME_DELAY_MS: libc::c_uint = 80;

#[derive(Clone, Copy)]
enum PixelFormat {
    Rgb565,
    Bgra8888,
}

impl PixelFormat {
    fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Rgb565 => 2,
            Self::Bgra8888 => 4,
        }
    }
}

pub(super) struct FbFile {
    fd: libc::c_int,
    fixed_info: libc::fb_fix_screeninfo,
    variable_info: libc::fb_var_screeninfo,
    pixel_format: PixelFormat,
}

/// Renders into one reusable scanline instead of allocating a full-frame
/// pixel buffer.
///
/// This keeps the software renderer's SRAM usage low. Slint 1.17 does not
/// support `Path` items in `render_by_line`, so this backend intentionally
/// does not accept `Path` items.
pub(super) struct FbLineBuffer<'a> {
    fb: &'a mut FbFile,
    pixels: [Rgb565Pixel; LCD_H_RES as usize],
    result: &'a mut IoResult<()>,
}

impl<'a> FbLineBuffer<'a> {
    pub(super) fn new(fb: &'a mut FbFile, result: &'a mut IoResult<()>) -> Self {
        Self {
            fb,
            pixels: [Rgb565Pixel(0); LCD_H_RES as usize],
            result,
        }
    }
}

impl LineBufferProvider for FbLineBuffer<'_> {
    type TargetPixel = Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [Self::TargetPixel]),
    ) {
        let pixel_count = range.len();
        debug_assert!(pixel_count <= self.pixels.len());

        let pixels = &mut self.pixels[..pixel_count];
        render_fn(pixels);

        if self.result.is_ok() {
            *self.result = self.fb.draw_line(pixels, range.start, line);
        }
    }
}

impl Drop for FbFile {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.fd);
    }
}

unsafe fn ioctl(fd: libc::c_int, request: libc::c_ulong, arg: *mut libc::c_void) -> IoResult<()> {
    match librs::syscall::sys::Sys::ioctl(fd, request, arg) {
        Ok(ret) if ret < 0 => Err(syscall_error(ret)),
        Ok(_) => Ok(()),
        Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
    }
}

impl FbFile {
    pub(super) fn open() -> IoResult<Self> {
        let path = CStr::from_bytes_with_nul(b"/dev/fb0\0")
            .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
        let fd = librs::syscall::sys::Sys::open(path, libc::O_RDWR, 0);
        if fd < 0 {
            return Err(syscall_error(fd));
        }

        let mut fb = Self {
            fd,
            fixed_info: unsafe { core::mem::zeroed() },
            variable_info: unsafe { core::mem::zeroed() },
            pixel_format: PixelFormat::Rgb565,
        };

        if let Err(err) = fb.load_info().and_then(|_| fb.validate_format()) {
            return Err(err);
        }

        Ok(fb)
    }

    fn load_info(&mut self) -> IoResult<()> {
        unsafe {
            ioctl(
                self.fd,
                libc::FBIOGET_FSCREENINFO,
                &mut self.fixed_info as *mut libc::fb_fix_screeninfo as *mut libc::c_void,
            )?;
            ioctl(
                self.fd,
                libc::FBIOGET_VSCREENINFO,
                &mut self.variable_info as *mut libc::fb_var_screeninfo as *mut libc::c_void,
            )?;
        }
        Ok(())
    }

    fn validate_format(&mut self) -> IoResult<()> {
        let info = &self.variable_info;
        let fixed = &self.fixed_info;
        let pixel_format = if is_rgb565(info) {
            PixelFormat::Rgb565
        } else if is_bgra8888(info) {
            PixelFormat::Bgra8888
        } else {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "unsupported framebuffer format",
            ));
        };
        let min_line_length = info
            .xres
            .checked_mul(pixel_format.bytes_per_pixel())
            .ok_or_else(|| Error::from_raw_os_error(libc::EINVAL))?;
        let min_size = fixed
            .line_length
            .checked_mul(info.yres)
            .ok_or_else(|| Error::from_raw_os_error(libc::EINVAL))?;

        if info.xres < LCD_H_RES as u32
            || info.yres < LCD_V_RES as u32
            || fixed.line_length < min_line_length
            || fixed.smem_len < min_size
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "unsupported framebuffer format",
            ));
        }

        self.pixel_format = pixel_format;
        Ok(())
    }

    fn draw_line(
        &mut self,
        pixels: &[Rgb565Pixel],
        origin_x: usize,
        origin_y: usize,
    ) -> IoResult<()> {
        let dst_offset = origin_y as u64 * self.fixed_info.line_length as u64
            + origin_x as u64 * self.pixel_format.bytes_per_pixel() as u64;
        if dst_offset > libc::off_t::MAX as u64 {
            return Err(Error::from_raw_os_error(libc::EINVAL));
        }

        let offset =
            librs::syscall::sys::Sys::lseek(self.fd, dst_offset as libc::off_t, libc::SEEK_SET);
        if offset < 0 {
            return Err(syscall_error(offset as libc::c_int));
        }

        match self.pixel_format {
            PixelFormat::Rgb565 => write_rgb565_line(self.fd, pixels),
            PixelFormat::Bgra8888 => write_bgra8888_line(self.fd, pixels),
        }
    }
}

fn write_all(fd: libc::c_int, mut buf: &[u8]) -> IoResult<()> {
    while !buf.is_empty() {
        match librs::syscall::sys::Sys::write(fd, buf) {
            Ok(0) => {
                return Err(Error::new(
                    ErrorKind::WriteZero,
                    "failed to write framebuffer",
                ))
            }
            Ok(size) => buf = &buf[size..],
            Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
        }
    }

    Ok(())
}

fn write_rgb565_line(fd: libc::c_int, pixels: &[Rgb565Pixel]) -> IoResult<()> {
    let mut bytes = [0; 128];
    let mut used = 0;

    for pixel in pixels {
        let pixel_bytes = pixel.0.to_be_bytes();
        bytes[used] = pixel_bytes[0];
        bytes[used + 1] = pixel_bytes[1];
        used += 2;

        if used == bytes.len() {
            write_all(fd, &bytes)?;
            used = 0;
        }
    }

    if used > 0 {
        write_all(fd, &bytes[..used])?;
    }

    Ok(())
}

fn write_bgra8888_line(fd: libc::c_int, pixels: &[Rgb565Pixel]) -> IoResult<()> {
    let mut bytes = [0; 128];
    let mut used = 0;

    for pixel in pixels {
        // Rgb565Pixel.0 is the u16 RGB565 value (big-endian on the wire
        // only in write_rgb565_line; here the value is host-order).
        let rgb = pixel.0;
        bytes[used] = ((rgb & 0x001f) << 3) as u8;
        bytes[used + 1] = ((rgb & 0x07e0) >> 3) as u8;
        bytes[used + 2] = ((rgb & 0xf800) >> 8) as u8;
        bytes[used + 3] = 0xff;
        used += 4;

        if used == bytes.len() {
            write_all(fd, &bytes)?;
            used = 0;
        }
    }

    if used > 0 {
        write_all(fd, &bytes[..used])?;
    }

    Ok(())
}

fn is_rgb565(info: &libc::fb_var_screeninfo) -> bool {
    info.bits_per_pixel == 16
        && info.red.offset == 11
        && info.red.length == 5
        && info.green.offset == 5
        && info.green.length == 6
        && info.blue.offset == 0
        && info.blue.length == 5
}

fn is_bgra8888(info: &libc::fb_var_screeninfo) -> bool {
    info.bits_per_pixel == 32
        && info.red.offset == 16
        && info.red.length == 8
        && info.green.offset == 8
        && info.green.length == 8
        && info.blue.offset == 0
        && info.blue.length == 8
}

fn syscall_error(ret: libc::c_int) -> Error {
    if ret == -1 {
        Error::last_os_error()
    } else {
        Error::from_raw_os_error(-ret)
    }
}

/// Milliseconds since boot on the kernel's CLOCK_MONOTONIC (which is 1 on
/// BlueOS, not the 4 that Rust std's Instant would use).
pub(super) fn uptime_millis() -> u128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    let ret = unsafe { librs::time::clock_gettime(librs::time::CLOCK_MONOTONIC, &mut ts) };

    if ret != 0 {
        return 0;
    }

    (ts.tv_sec as u128) * 1000 + (ts.tv_nsec as u128) / 1_000_000
}
