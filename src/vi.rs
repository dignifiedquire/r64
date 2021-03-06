extern crate byteorder;
extern crate emu;
extern crate slog;
use emu::bus::be::{Bus, Reg32};
use emu::gfx::*;
use emu::int::Numerics;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(DeviceBE)]
pub struct Vi {
    // [1:0] type[1:0] (pixel size)
    //     0: blank (no data, no sync)
    //     1: reserved
    //     2: 5/5/5/3 ("16" bit)
    //     3: 8/8/8/8 (32 bit)
    // [2] gamma_dither_enable (normally on, unless "special effect")
    // [3] gamma_enable (normally on, unless MPEG/JPEG)
    // [4] divot_enable (normally on if antialiased,
    //     unless decal lines)
    // [5] reserved - always off
    // [6] serrate (always on if interlaced, off if not)
    // [7] reserved - diagnostics only
    // [9:8] anti-alias (aa) mode[1:0]
    //     0: aa & resamp (always fetch extra lines)
    //     1: aa & resamp (fetch extra lines if needed)
    //     2: resamp only (treat as all fully covered)
    //     3: neither (replicate pixels, no interpolate)
    // [11] reserved - diagnostics only
    // [15:12] reserved
    #[reg(offset = 0x00, rwmask = 0xFFFF)]
    status: Reg32,

    // [23:0] frame buffer origin in bytes
    #[reg(offset = 0x04, rwmask = 0xFFFFFF)]
    origin: Reg32,

    // [11:0] frame buffer line width in pixels
    #[reg(offset = 0x08, rwmask = 0xFFF)]
    width: Reg32,

    // [9:0] interrupt when current half-line = V_INTR
    #[reg(offset = 0x0C, rwmask = 0x3FF)]
    vertical_interrupt: Reg32,

    // [9:0] current half line, sampled once per line (the lsb of
    //       V_CURRENT is constant within a field, and in
    //       interlaced modes gives the field number - which is
    //       constant for non-interlaced modes)
    //       - Writes clears interrupt line
    #[reg(offset = 0x10, rwmask = 0, wcb)]
    current_line: Reg32,

    // [7:0] horizontal sync width in pixels
    // [15:8] color burst width in pixels
    // [19:16] vertical sync width in half lines
    // [29:20] start of color burst in pixels from h-sync
    #[reg(offset = 0x14, rwmask = 0x3FFFFFFF)]
    timing: Reg32,

    // [9:0] number of half-lines per field
    #[reg(offset = 0x18)]
    vertical_sync: Reg32,

    // [11:0] total duration of a line in 1/4 pixel
    // [20:16] a 5-bit leap pattern used for PAL only (h_sync_period)
    #[reg(offset = 0x1C, rwmask = 0x1FFFFF)]
    horizontal_sync: Reg32,

    // [11:0] identical to h_sync_period
    // [27:16] identical to h_sync_period
    #[reg(offset = 0x20, rwmask = 0xFFFFFFF)]
    horizontal_sync_leap: Reg32,

    // [9:0] end of active video in screen pixels
    // [25:16] start of active video in screen pixels
    #[reg(offset = 0x24, rwmask = 0x3FFFFFF)]
    horizontal_video: Reg32,

    // [9:0] end of active video in screen half-lines
    // [25:16] start of active video in screen half-lines
    #[reg(offset = 0x28, rwmask = 0x3FFFFFF)]
    vertical_video: Reg32,

    // [9:0] end of color burst enable in half-lines
    // [25:16] start of color burst enable in half-lines
    #[reg(offset = 0x2C, rwmask = 0x3FFFFFF)]
    vertical_burst: Reg32,

    // [11:0] 1/horizontal scale up factor (2.10 format)
    // [27:16] horizontal subpixel offset (2.10 format)
    #[reg(offset = 0x30, rwmask = 0xFFFFFFF)]
    x_scale: Reg32,

    // [11:0] 1/vertical scale up factor (2.10 format)
    // [27:16] vertical subpixel offset (2.10 format)
    #[reg(offset = 0x34, rwmask = 0xFFFFFFF)]
    y_scale: Reg32,

    logger: slog::Logger,
    bus: Rc<RefCell<Box<Bus>>>,
}

impl Vi {
    pub fn new(logger: slog::Logger, bus: Rc<RefCell<Box<Bus>>>) -> Vi {
        Vi {
            status: Reg32::default(),
            origin: Reg32::default(),
            width: Reg32::default(),
            vertical_interrupt: Reg32::default(),
            current_line: Reg32::default(),
            timing: Reg32::default(),
            vertical_sync: Reg32::default(),
            horizontal_sync: Reg32::default(),
            horizontal_sync_leap: Reg32::default(),
            horizontal_video: Reg32::default(),
            vertical_video: Reg32::default(),
            vertical_burst: Reg32::default(),
            x_scale: Reg32::default(),
            y_scale: Reg32::default(),
            logger,
            bus,
        }
    }

    pub fn set_line(&self, y: usize) {
        self.current_line.set(y as u32);
    }

    fn cb_write_current_line(&self, _old: u32, new: u32) {
        error!(self.logger, "write VI current line"; o!("val" => new.hex()));
    }

    pub fn draw_frame(&self, screen: &mut GfxBufferMutLE<Rgb888>) {
        let bpp = self.status.get() & 3;

        // display disable -> clear screen
        if bpp == 0 || bpp == 1 {
            let black = Color::<Rgb888>::new_clamped(0, 0, 0, 0);
            for y in 0..480 {
                let mut line = screen.line(y);
                for x in 0..640 {
                    line.set(x, black);
                }
            }
            return;
        }

        info!(self.logger, "draw frame"; o!("origin" => self.origin.get().hex()));
        let memio = self.bus.borrow().fetch_read::<u8>(self.origin.get());
        let src = memio.mem().unwrap();

        match self.width.get() {
            640 => {
                let src = GfxBufferLE::<Rgb888>::new(src, 640, 480, 640 * 4).unwrap();
                for y in 0..480 {
                    let mut dst = screen.line(y);
                    let src = src.line(y);
                    for x in 0..640 {
                        dst.set(x, src.get(x));
                    }
                }
            }

            320 => {
                match bpp {
                    // 32-bit
                    3 => {
                        let src = GfxBufferLE::<Rgb888>::new(src, 320, 240, 320 * 4).unwrap();
                        for y in 0..240 {
                            let (mut dst1, mut dst2) = screen.lines(y * 2, y * 2 + 1);
                            let src = src.line(y);
                            for x in 0..320 {
                                let px = src.get(x);
                                dst1.set(x * 2, px);
                                dst1.set(x * 2 + 1, px);
                                dst2.set(x * 2, px);
                                dst2.set(x * 2 + 1, px);
                            }
                        }
                    }
                    // 16-bit
                    2 => {
                        let src = GfxBufferLE::<Rgb555>::new(src, 320, 240, 320 * 2).unwrap();
                        for y in 0..240 {
                            let (mut dst1, mut dst2) = screen.lines(y * 2, y * 2 + 1);
                            let src = src.line(y);
                            for x in 0..320 {
                                let px = src.get(x).cconv();
                                dst1.set(x * 2, px);
                                dst1.set(x * 2 + 1, px);
                                dst2.set(x * 2, px);
                                dst2.set(x * 2 + 1, px);
                            }
                        }
                    }
                    _ => unimplemented!(),
                }
            }

            _ => {
                error!(self.logger, "unsupported screen width"; o!("width" => self.width.get()));
            }
        }
    }
}
