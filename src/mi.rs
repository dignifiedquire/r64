extern crate emu;
extern crate slog;

use bit_field::BitField;
use emu::bus::be::Reg32;
use mips64;
use std::cell::RefCell;
use std::rc::Rc;

/// The line that all RSP interrupts go to on the CPU.
const RSP_LINE: mips64::Line = mips64::Line::IP2;

pub enum Line {
    SP = 0,
    SI = 1,
    AI = 2,
    VI = 3,
    PI = 4,
    DP = 5,
}

#[derive(DeviceBE)]
pub struct Mi {
    // (W): [6:0] init length        (R): [6:0] init length
    //      [7] clear init mode           [7] init mode
    //      [8] set init mode             [8] ebus test mode
    //      [9/10] clr/set ebus test mode [9] RDRAM reg mode
    //      [11] clear DP interrupt
    //      [12] clear RDRAM reg
    //      [13] set RDRAM reg mode
    #[reg(
        bank = 0,
        offset = 0x00,
        rwmask = 0x3FFF,
        init = 0x80,
        wcb,
        rcb
    )]
    init_mode: Reg32,

    // (R): [7:0] io
    //      [15:8] rac
    //      [23:16] rdp
    //      [31:24] rsp
    #[reg(
        bank = 0,
        offset = 0x04,
        rwmask = 0,
        init = 0x01010101,
        readonly
    )]
    version: Reg32,

    // (R): [0] SP intr
    //      [1] SI intr
    //      [2] AI intr
    //      [3] VI intr
    //      [4] PI intr
    //      [5] DP intr
    #[reg(bank = 0, offset = 0x08, rwmask = 0x3F, readonly)]
    interrupt: Reg32,

    // (W): [0/1] clear/set SP mask  (R): [0] SP intr mask
    //      [2/3] clear/set SI mask       [1] SI intr mask
    //      [4/5] clear/set AI mask       [2] AI intr mask
    //      [6/7] clear/set VI mask       [3] VI intr mask
    //      [8/9] clear/set PI mask       [4] PI intr mask
    //      [10/11] clear/set DP mask     [5] DP intr mask
    #[reg(bank = 0, offset = 0x0C, rwmask = 0xFFF, wcb, rcb)]
    interrupt_mask: Reg32,

    logger: slog::Logger,
    cpu: Rc<RefCell<Box<mips64::Cpu>>>,
}

impl Mi {
    pub fn new(logger: slog::Logger, cpu: Rc<RefCell<Box<mips64::Cpu>>>) -> Mi {
        Mi {
            init_mode: Reg32::default(),
            version: Reg32::default(),
            interrupt: Reg32::default(),
            interrupt_mask: Reg32::default(),

            logger,
            cpu,
        }
    }

    pub fn set_line(&mut self, line: Line, val: bool) {
        self.interrupt
            .set(*self.interrupt.get().set_bit(line as usize, val));
        self.update_interrupts();
    }

    /// Changes the lines sent to the CPU, depending on the current state.
    fn update_interrupts(&self) {
        let val = self.interrupt.get() & self.interrupt_mask.get() > 0;

        self.cpu.borrow_mut().ctx_mut().set_line(RSP_LINE, val);
    }

    fn cb_write_init_mode(&mut self, old: u32, new: u32) {
        let mut res = old;

        // init length
        res.set_bits(0..7, new.get_bits(0..7));

        // clear init mode
        if new.get_bit(7) {
            res.set_bit(7, false);
        }

        // set init mode
        if new.get_bit(8) {
            res.set_bit(7, true);
        }

        // clear ebus test mode
        if new.get_bit(9) {
            res.set_bit(8, false);
        }

        // set ebus test mode
        if new.get_bit(10) {
            res.set_bit(8, true);
        }

        // clear DP interrupt
        if new.get_bit(11) {
            self.set_line(Line::DP, false);
        }

        // clear RDRAM reg mode
        if new.get_bit(12) {
            res.set_bit(9, false);
        }

        // set RDRAM reg mode
        if new.get_bit(13) {
            res.set_bit(9, true);
        }

        self.init_mode.set(res);
    }

    fn cb_read_init_mode(&self, old: u32) -> u32 {
        old.get_bits(0..10)
    }

    fn cb_write_interrupt_mask(&mut self, old: u32, new: u32) {
        let mut res = old;

        // clear SP mask
        if new.get_bit(0) {
            res.set_bit(0, false);
        }
        // set SP mask
        if new.get_bit(1) {
            res.set_bit(0, true);
        }

        // clear SI mask
        if new.get_bit(2) {
            res.set_bit(1, false);
        }
        // set SI mask
        if new.get_bit(3) {
            res.set_bit(1, true);
        }

        // clear AI mask
        if new.get_bit(4) {
            res.set_bit(2, false);
        }
        // set AI mask
        if new.get_bit(5) {
            res.set_bit(2, true);
        }

        // clear VI mask
        if new.get_bit(6) {
            res.set_bit(3, false);
        }
        // set VI mask
        if new.get_bit(7) {
            res.set_bit(3, true);
        }

        // clear PI mask
        if new.get_bit(8) {
            res.set_bit(3, false);
        }
        // set PI mask
        if new.get_bit(9) {
            res.set_bit(4, true);
        }

        // clear DP mask
        if new.get_bit(10) {
            res.set_bit(5, false);
        }
        // set DP mask
        if new.get_bit(11) {
            res.set_bit(5, true);
        }

        self.init_mode.set(res);
        self.update_interrupts();
    }

    fn cb_read_interrupt_mask(&self, old: u32) -> u32 {
        old.get_bits(0..6)
    }
}

#[cfg(test)]
mod tests {
    use super::emu::bus::{Bus, DevPtr};
    use super::slog;
    use super::slog::Drain;
    use super::*;
    use bit_field::BitField;
    extern crate slog_term;
    use std;

    fn logger() -> slog::Logger {
        let decorator = slog_term::PlainSyncDecorator::new(std::io::stdout());
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        slog::Logger::root(drain, o!())
    }

    #[test]
    fn test_regs_mi() {
        let bus = Rc::new(RefCell::new(Bus::new(logger())));
        let cpu = Rc::new(RefCell::new(Box::new(mips64::Cpu::new(
            logger(),
            bus.clone(),
        ))));

        let mi = DevPtr::new(Mi::new(logger(), cpu.clone()));
        let mut bus = bus.borrow_mut();
        bus.map_device(0x00, &mi, 0).unwrap();

        // setting everything to 0
        bus.write::<u32>(0x00, 0x00);

        // init mode is set by default
        assert_eq!(bus.read::<u32>(0x00).get_bit(7), true);

        // clear init mode
        bus.write::<u32>(0x00, *0u32.set_bit(7, true));
        assert_eq!(bus.read::<u32>(0x00).get_bit(7), false);

        // setting init mode
        let val = *0u32.set_bit(8, true);
        bus.write::<u32>(0x00, val);
        assert_eq!(bus.read::<u32>(0x00).get_bit(7), true);

        // setting rdram reg mode
        let val = *0u32.set_bit(13, true);
        bus.write::<u32>(0x00, val);
        assert_eq!(bus.read::<u32>(0x00).get_bit(9), true);

        // clear rdram reg mode
        bus.write::<u32>(0x00, *0u32.set_bit(12, true));
        assert_eq!(bus.read::<u32>(0x00).get_bit(9), false);

        // write init mode
        bus.write::<u32>(0x00, *0u32.set_bits(0..7, 0xF));
        assert_eq!(bus.read::<u32>(0x00).get_bits(0..7), 0xF);
    }
}
