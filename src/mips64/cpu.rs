extern crate emu;

use self::emu::bus::be::{Bus, MemIoR};
use self::emu::bus::MemInt;
use self::emu::int::Numerics;
use self::emu::sync;
use slog;
use std::cell::RefCell;
use std::rc::Rc;

// -- Lines

// -- CPU Cause Reg

// IP0 Bit  8 - Software 1 	CAUSE_SW1 	Software generated interrupt 1
// IP1 Bit  9 - Software 2 	CAUSE_SW2 	Software generated interrupt 2
// IP2 Bit 10 - RCP 	        CAUSE_IP3 	RCP interrupt asserted
// IP3 Bit 11 - Cartridge 	CAUSE_IP4 	A peripheral has generated an interrupt
// IP4 Bit 12 - Pre-nmi 	CAUSE_IP5 	User has pushed reset button on console
// IP5 Bit 13 - RDB Read 	CAUSE_IP6 	Indy has read the value in the RDB port
// IP6 Bit 14 - RDB Write 	CAUSE_IP7 	Indy has written a value to the RDB port
// IP7 Bit 15 - Counter 	CAUSE_IP8 	Internal counter has reached its terminal count

// -- CPU Interrupt Reg
// can be written to by external hardware
//
// Bit [0..4] - IP0 - IP4
// Bit 6  - Pin NMI

// interrupt reg + pins are or'ed together => cause reg

pub enum Line {
    // Software Interrupts
    // [n64-os] SW1
    IP0 = 0b0000_0001,
    // [n64-os] SW2
    IP1 = 0b0000_0010,

    // External Interrupts
    // [n64-os] IP3 - RCP
    IP2 = 0b0000_0100,
    // [n64-os] IP4 - Cartridge
    IP3 = 0b0000_1000,
    // [n64-os]
    IP4 = 0b0001_0000,
    // [n64-os] SW1
    IP5 = 0b0010_0000,

    // NMI
    IP6 = 0b0100_0000,

    // Timer Interrupt
    IP7 = 0b1000_0000,
}

/// Cop is a MIPS64 coprocessor that can be installed within the core.
pub trait Cop {
    fn reg(&self, idx: usize) -> u128;
    fn set_reg(&mut self, idx: usize, val: u128);

    fn op(&mut self, cpu: &mut CpuContext, opcode: u32);

    fn lwc(&mut self, op: u32, ctx: &CpuContext, bus: &Rc<RefCell<Box<Bus>>>) {
        let rt = ((op >> 16) & 0x1f) as usize;
        let ea = ctx.regs[((op >> 21) & 0x1f) as usize] as u32 + (op & 0xffff) as i16 as i32 as u32;
        let val = bus.borrow().read::<u32>(ea & 0x1FFF_FFFC) as u64;
        self.set_reg(rt, val as u128);
    }

    fn ldc(&mut self, op: u32, ctx: &CpuContext, bus: &Rc<RefCell<Box<Bus>>>) {
        let rt = ((op >> 16) & 0x1f) as usize;
        let ea = ctx.regs[((op >> 21) & 0x1f) as usize] as u32 + (op & 0xffff) as i16 as i32 as u32;
        let val = bus.borrow().read::<u64>(ea & 0x1FFF_FFFC) as u64;
        self.set_reg(rt, val as u128);
    }

    fn swc(&mut self, op: u32, ctx: &CpuContext, bus: &Rc<RefCell<Box<Bus>>>) {
        let rt = ((op >> 16) & 0x1f) as usize;
        let ea = ctx.regs[((op >> 21) & 0x1f) as usize] as u32 + (op & 0xffff) as i16 as i32 as u32;
        let val = self.reg(rt) as u32;
        bus.borrow().write::<u32>(ea & 0x1FFF_FFFC, val);
    }

    fn sdc(&mut self, op: u32, ctx: &CpuContext, bus: &Rc<RefCell<Box<Bus>>>) {
        let rt = ((op >> 16) & 0x1f) as usize;
        let ea = ctx.regs[((op >> 21) & 0x1f) as usize] as u32 + (op & 0xffff) as i16 as i32 as u32;
        let val = self.reg(rt) as u64;
        bus.borrow().write::<u64>(ea & 0x1FFF_FFFC, val);
    }
}

pub enum Exception {
    INT = 0x00,  // Interrupt
    MOD = 0x01,  // TLB modification exception
    TLBL = 0x02, // TLB load/fetch
    TLBS = 0x03, // TLB store
    ADEL = 0x04, // Address error (load/fetch)
    ADES = 0x05, // Address error (store)
    SYS = 0x08,  // Syscall
    BP = 0x09,   // Breakpoint
    RI = 0x0A,   // Reserved instruction

    // Special exceptions that are not specified in the Cause register
    RESET = 0x100,
    SOFTRESET = 0x101,
    NMI = 0x102,
}

/// Cop0 is a MIPS64 coprocessor #0, which (in addition to being a normal coprocessor)
/// it is able to control execution of the core by triggering exceptions.
pub trait Cop0: Cop {
    /// Check if there's a pending interrupt. It is expected that if this
    /// function returns true, Cop0::exception() is immediately called with
    /// exc == Exception::Int.
    fn pending_int(&self) -> bool;

    /// Trigger the specified excepion.
    fn exception(&mut self, ctx: &mut CpuContext, exc: Exception);
}

pub struct CpuContext {
    pub regs: [u64; 32],
    pub hi: u64,
    pub lo: u64,
    pub(crate) pc: u32,
    pub(crate) branch_pc: u32,
    pub clock: i64,
    pub tight_exit: bool,
    lines: u8,
}

pub struct Cpu {
    ctx: CpuContext,

    cop0: Option<Box<Cop0>>,
    cop1: Option<Box<Cop>>,
    cop2: Option<Box<Cop>>,
    cop3: Option<Box<Cop>>,

    logger: slog::Logger,
    bus: Rc<RefCell<Box<Bus>>>,
    until: i64,

    last_fetch_addr: u32,
    last_fetch_mem: MemIoR<u32>,
}

struct Mipsop<'a> {
    opcode: u32,
    cpu: &'a mut Cpu,
}

impl<'a> Mipsop<'a> {
    fn op(&self) -> u32 {
        self.opcode >> 26
    }
    fn special(&self) -> u32 {
        self.opcode & 0x3f
    }
    fn ea(&self) -> u32 {
        self.rs32() + self.sximm32() as u32
    }
    fn sa(&self) -> usize {
        ((self.opcode >> 6) & 0x1f) as usize
    }
    fn btgt(&self) -> u32 {
        self.cpu.ctx.pc + self.sximm32() as u32 * 4
    }
    fn jtgt(&self) -> u32 {
        (self.cpu.ctx.pc & 0xF000_0000) + ((self.opcode & 0x03FF_FFFF) * 4)
    }
    fn rs(&self) -> usize {
        ((self.opcode >> 21) & 0x1f) as usize
    }
    fn rt(&self) -> usize {
        ((self.opcode >> 16) & 0x1f) as usize
    }
    fn rd(&self) -> usize {
        ((self.opcode >> 11) & 0x1f) as usize
    }
    fn sximm32(&self) -> i32 {
        (self.opcode & 0xffff) as i16 as i32
    }
    fn sximm64(&self) -> i64 {
        (self.opcode & 0xffff) as i16 as i64
    }
    fn imm64(&self) -> u64 {
        (self.opcode & 0xffff) as u64
    }
    fn rs64(&self) -> u64 {
        self.cpu.ctx.regs[self.rs()]
    }
    fn rt64(&self) -> u64 {
        self.cpu.ctx.regs[self.rt()]
    }
    fn irs64(&self) -> i64 {
        self.rs64() as i64
    }
    fn irt64(&self) -> i64 {
        self.rt64() as i64
    }
    fn rs32(&self) -> u32 {
        self.rs64() as u32
    }
    fn rt32(&self) -> u32 {
        self.rt64() as u32
    }
    fn irs32(&self) -> i32 {
        self.rs64() as i32
    }
    fn irt32(&self) -> i32 {
        self.rt64() as i32
    }
    fn mrt64(&'a mut self) -> &'a mut u64 {
        &mut self.cpu.ctx.regs[self.rt()]
    }
    fn mrd64(&'a mut self) -> &'a mut u64 {
        &mut self.cpu.ctx.regs[self.rd()]
    }
    fn hex(&self) -> String {
        format!("{:x}", self.opcode)
    }
}

impl CpuContext {
    #[inline]
    pub fn branch(&mut self, cond: bool, tgt: u32, likely: bool) {
        if cond {
            self.branch_pc = tgt;
            self.tight_exit = true;
        } else if likely {
            // branch not taken; if likely, skip delay slot
            self.pc += 4;
            self.clock += 1;
            self.tight_exit = true;
        }
    }

    pub fn set_line(&mut self, line: Line, stat: bool) {
        let line_val = line as u8;
        if stat {
            // check activation of a new line
            if self.lines | line_val != 0 {
                self.tight_exit = true;
            }
            self.lines |= line_val;
        } else {
            self.lines &= line_val;
        }
    }

    pub fn set_pc(&mut self, pc: u32) {
        self.pc = pc;
        self.branch_pc = 0;
    }

    pub fn get_pc(&self) -> u32 {
        self.pc
    }
}

macro_rules! branch {
    ($op:ident, $cond:expr, $tgt:expr) => {{
        branch!($op, $cond, $tgt, link(false), likely(false));
    }};
    ($op:ident, $cond:expr, $tgt:expr,link($link:expr)) => {{
        branch!($op, $cond, $tgt, link($link), likely(true));
    }};
    ($op:ident, $cond:expr, $tgt:expr,likely($lkl:expr)) => {{
        branch!($op, $cond, $tgt, link(false), likely($lkl));
    }};
    ($op:ident, $cond:expr, $tgt:expr,link($link:expr),likely($lkl:expr)) => {{
        if $link {
            $op.cpu.ctx.regs[31] = ($op.cpu.ctx.pc + 4) as u64;
        }
        let (cond, tgt) = ($cond, $tgt);
        $op.cpu.ctx.branch(cond, tgt, $lkl);
    }};
}

macro_rules! check_overflow_add {
    ($op:ident, $dest:expr, $reg1:expr, $reg2:expr) => {{
        match $reg1.checked_add($reg2) {
            Some(res) => $dest = res.sx64(),
            None => $op.cpu.trap_overflow(),
        }
    }};
}

macro_rules! check_overflow_sub {
    ($op:ident, $dest:expr, $reg1:expr, $reg2:expr) => {{
        match $reg1.checked_sub($reg2) {
            Some(res) => $dest = res.sx64(),
            None => $op.cpu.trap_overflow(),
        }
    }};
}

macro_rules! if_cop {
    ($op:ident, $cop:ident, $do:expr) => {{
        match $op.cpu.$cop {
            Some(ref mut $cop) => $do,
            None => {
                let pc = $op.cpu.ctx.pc;
                let opcode = $op.opcode;
                warn!($op.cpu.logger, "COP opcode without COP"; o!("pc" => pc.hex(), "op" => opcode.hex()));
            }
        }
    }};
}

impl Cpu {
    pub fn new(logger: slog::Logger, bus: Rc<RefCell<Box<Bus>>>) -> Cpu {
        return Cpu {
            ctx: CpuContext {
                regs: [0u64; 32],
                hi: 0,
                lo: 0,
                pc: 0x1FC0_0000, // FIXME
                branch_pc: 0,
                clock: 0,
                tight_exit: false,
                lines: 0,
            },
            bus: bus,
            cop0: None,
            cop1: None,
            cop2: None,
            cop3: None,
            logger: logger,
            until: 0,
            last_fetch_addr: 0xFFFF_FFFF,
            last_fetch_mem: MemIoR::default(),
        };
    }

    pub fn ctx(&self) -> &CpuContext {
        &self.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut CpuContext {
        &mut self.ctx
    }

    pub fn set_cop0(&mut self, cop0: Box<dyn Cop0>) {
        self.cop0 = Some(cop0);
    }
    pub fn set_cop1(&mut self, cop1: Box<dyn Cop>) {
        self.cop1 = Some(cop1);
    }
    pub fn set_cop2(&mut self, cop2: Box<dyn Cop>) {
        self.cop2 = Some(cop2);
    }
    pub fn set_cop3(&mut self, cop3: Box<dyn Cop>) {
        self.cop3 = Some(cop3);
    }
    pub fn cop2(&mut self) -> Option<&mut Box<dyn Cop>> {
        self.cop2.as_mut()
    }

    pub fn reset(&mut self) {
        self.exception(Exception::RESET);
    }

    fn exception(&mut self, exc: Exception) {
        if let Some(ref mut cop0) = self.cop0 {
            cop0.exception(&mut self.ctx, exc);
        }
    }

    fn trap_overflow(&mut self) {
        unimplemented!();
    }

    fn op(&mut self, opcode: u32) {
        self.ctx.clock += 1;
        let mut op = Mipsop { opcode, cpu: self };
        match op.op() {
            // SPECIAL
            0x00 => match op.special() {
                0x00 => *op.mrd64() = (op.rt32() << op.sa()).sx64(), // SLL
                0x02 => *op.mrd64() = (op.rt32() >> op.sa()).sx64(), // SRL
                0x03 => *op.mrd64() = (op.irt32() >> op.sa()).sx64(), // SRA
                0x04 => *op.mrd64() = (op.rt32() << (op.rs32() & 0x1F)).sx64(), // SLLV
                0x06 => *op.mrd64() = (op.rt32() >> (op.rs32() & 0x1F)).sx64(), // SRLV
                0x07 => *op.mrd64() = (op.irt32() >> (op.rs32() & 0x1F)).sx64(), // SRAV
                0x08 => branch!(op, true, op.rs32(), link(false)),   // JR
                0x09 => branch!(op, true, op.rs32(), link(true)),    // JALR
                0x0D => op.cpu.exception(Exception::BP),             // BREAK
                0x0F => {}                                           // SYNC

                0x10 => *op.mrd64() = op.cpu.ctx.hi, // MFHI
                0x11 => op.cpu.ctx.hi = op.rs64(),   // MTHI
                0x12 => *op.mrd64() = op.cpu.ctx.lo, // MFLO
                0x13 => op.cpu.ctx.lo = op.rs64(),   // MTLO
                0x14 => *op.mrd64() = op.rt64() << (op.rs32() & 0x3F), // DSLLV
                0x16 => *op.mrd64() = op.rt64() >> (op.rs32() & 0x3F), // DSRLV
                0x17 => *op.mrd64() = (op.irt64() >> (op.rs32() & 0x3F)) as u64, // DSRAV
                0x18 => {
                    // MULT
                    let (hi, lo) =
                        (i64::wrapping_mul(op.rt32().isx64(), op.rs32().isx64()) as u64).hi_lo();
                    op.cpu.ctx.lo = lo;
                    op.cpu.ctx.hi = hi;
                }
                0x19 => {
                    // MULTU
                    let (hi, lo) = u64::wrapping_mul(op.rt32() as u64, op.rs32() as u64).hi_lo();
                    op.cpu.ctx.lo = lo;
                    op.cpu.ctx.hi = hi;
                }
                0x1A => {
                    // DIV
                    op.cpu.ctx.lo = op.irs32().wrapping_div(op.irt32()).sx64();
                    op.cpu.ctx.hi = op.irs32().wrapping_rem(op.irt32()).sx64();
                }
                0x1B => {
                    // DIVU
                    op.cpu.ctx.lo = op.rs32().wrapping_div(op.rt32()).sx64();
                    op.cpu.ctx.hi = op.rs32().wrapping_rem(op.rt32()).sx64();
                }
                0x1C => {
                    // DMULT
                    let (hi, lo) =
                        i128::wrapping_mul(op.irt64() as i128, op.irs64() as i128).hi_lo();
                    op.cpu.ctx.lo = lo as u64;
                    op.cpu.ctx.hi = hi as u64;
                }
                0x1D => {
                    // DMULTU
                    let (hi, lo) = u128::wrapping_mul(op.rt64() as u128, op.rs64() as u128).hi_lo();
                    op.cpu.ctx.lo = lo as u64;
                    op.cpu.ctx.hi = hi as u64;
                }
                0x1E => {
                    // DDIV
                    op.cpu.ctx.lo = op.irs64().wrapping_div(op.irt64()) as u64;
                    op.cpu.ctx.hi = op.irs64().wrapping_rem(op.irt64()) as u64;
                }
                0x1F => {
                    // DDIVU
                    op.cpu.ctx.lo = op.rs64().wrapping_div(op.rt64());
                    op.cpu.ctx.hi = op.rs64().wrapping_rem(op.rt64());
                }

                0x20 => check_overflow_add!(op, *op.mrd64(), op.irs32(), op.irt32()), // ADD
                0x21 => *op.mrd64() = (op.rs32() + op.rt32()).sx64(),                 // ADDU
                0x22 => check_overflow_sub!(op, *op.mrd64(), op.irs32(), op.irt32()), // SUB
                0x23 => *op.mrd64() = (op.rs32() - op.rt32()).sx64(),                 // SUBU
                0x24 => *op.mrd64() = op.rs64() & op.rt64(),                          // AND
                0x25 => *op.mrd64() = op.rs64() | op.rt64(),                          // OR
                0x26 => *op.mrd64() = op.rs64() ^ op.rt64(),                          // XOR
                0x27 => *op.mrd64() = !(op.rs64() | op.rt64()),                       // NOR
                0x2A => *op.mrd64() = (op.irs32() < op.irt32()) as u64,               // SLT
                0x2B => *op.mrd64() = (op.rs32() < op.rt32()) as u64,                 // SLTU
                0x2C => check_overflow_add!(op, *op.mrd64(), op.irs64(), op.irt64()), // DADD
                0x2D => *op.mrd64() = op.rs64() + op.rt64(),                          // DADDU
                0x2E => check_overflow_sub!(op, *op.mrd64(), op.irs64(), op.irt64()), // DSUB
                0x2F => *op.mrd64() = op.rs64() - op.rt64(),                          // DSUBU

                0x38 => *op.mrd64() = op.rt64() << op.sa(), // DSLL
                0x3A => *op.mrd64() = op.rt64() >> op.sa(), // DSRL
                0x3B => *op.mrd64() = (op.irt64() >> op.sa()) as u64, // DSRA
                0x3C => *op.mrd64() = op.rt64() << (op.sa() + 32), // DSLL32
                0x3E => *op.mrd64() = op.rt64() >> (op.sa() + 32), // DSRL32
                0x3F => *op.mrd64() = (op.irt64() >> (op.sa() + 32)) as u64, // DSRA32

                _ => panic!("unimplemented special opcode: func=0x{:x?}", op.special()),
            },

            // REGIMM
            0x01 => match op.rt() {
                0x00 => branch!(op, op.irs64() < 0, op.btgt(), link(false), likely(false)), // BLTZ
                0x01 => branch!(op, op.irs64() >= 0, op.btgt(), link(false), likely(false)), // BGEZ
                0x02 => branch!(op, op.irs64() < 0, op.btgt(), link(false), likely(true)),  // BLTZL
                0x03 => branch!(op, op.irs64() >= 0, op.btgt(), link(false), likely(true)), // BGEZL
                0x10 => branch!(op, op.irs64() < 0, op.btgt(), link(true), likely(false)), // BLTZAL
                0x11 => branch!(op, op.irs64() >= 0, op.btgt(), link(true), likely(false)), // BGEZAL
                0x12 => branch!(op, op.irs64() < 0, op.btgt(), link(true), likely(true)), // BLTZALL
                0x13 => branch!(op, op.irs64() >= 0, op.btgt(), link(true), likely(true)), // BGEZALL
                _ => panic!(
                    "unimplemented regimm opcode: func=0x{:x?} pc=0x{:x?}",
                    op.rt(),
                    op.cpu.ctx.pc - 4
                ),
            },

            0x02 => branch!(op, true, op.jtgt(), link(false)), // J
            0x03 => branch!(op, true, op.jtgt(), link(true)),  // JAL
            0x04 => branch!(op, op.rs64() == op.rt64(), op.btgt()), // BEQ
            0x05 => branch!(op, op.rs64() != op.rt64(), op.btgt()), // BNE
            0x06 => branch!(op, op.irs64() <= 0, op.btgt()),   // BLEZ
            0x07 => branch!(op, op.irs64() > 0, op.btgt()),    // BGTZ
            0x08 => check_overflow_add!(op, *op.mrt64(), op.irs32(), op.sximm32()), // ADDI
            0x09 => *op.mrt64() = (op.irs32() + op.sximm32()).sx64(), // ADDIU
            0x0A => *op.mrt64() = (op.irs32() < op.sximm32()) as u64, // SLTI
            0x0B => *op.mrt64() = (op.rs32() < op.sximm32() as u32) as u64, // SLTIU
            0x0C => *op.mrt64() = op.rs64() & op.imm64(),      // ANDI
            0x0D => *op.mrt64() = op.rs64() | op.imm64(),      // ORI
            0x0E => *op.mrt64() = op.rs64() ^ op.imm64(),      // XORI
            0x0F => *op.mrt64() = (op.sximm32() << 16).sx64(), // LUI

            0x10 => if_cop!(op, cop0, { cop0.op(&mut op.cpu.ctx, opcode) }), // COP0
            0x11 => if_cop!(op, cop1, { cop1.op(&mut op.cpu.ctx, opcode) }), // COP1
            0x12 => if_cop!(op, cop2, { cop2.op(&mut op.cpu.ctx, opcode) }), // COP2
            0x13 => if_cop!(op, cop3, { cop3.op(&mut op.cpu.ctx, opcode) }), // COP3
            0x14 => branch!(op, op.rs64() == op.rt64(), op.btgt(), likely(true)), // BEQL
            0x15 => branch!(op, op.rs64() != op.rt64(), op.btgt(), likely(true)), // BNEL
            0x16 => branch!(op, op.irs64() <= 0, op.btgt(), likely(true)),   // BLEZL
            0x17 => branch!(op, op.irs64() > 0, op.btgt(), likely(true)),    // BGTZL
            0x18 => check_overflow_add!(op, *op.mrt64(), op.irs64(), op.sximm64()), // DADDI
            0x19 => *op.mrt64() = (op.irs64() + op.sximm64()) as u64,        // DADDIU

            0x20 => *op.mrt64() = op.cpu.read::<u8>(op.ea()).sx64(), // LB
            0x21 => *op.mrt64() = op.cpu.read::<u16>(op.ea()).sx64(), // LH
            0x22 => *op.mrt64() = op.cpu.lwl(op.ea(), op.rt32()).sx64(), // LWL
            0x23 => *op.mrt64() = op.cpu.read::<u32>(op.ea()).sx64(), // LW
            0x24 => *op.mrt64() = op.cpu.read::<u8>(op.ea()) as u64, // LBU
            0x25 => *op.mrt64() = op.cpu.read::<u16>(op.ea()) as u64, // LHU
            0x26 => *op.mrt64() = op.cpu.lwr(op.ea(), op.rt32()).sx64(), // LWR
            0x27 => *op.mrt64() = op.cpu.read::<u32>(op.ea()) as u64, // LWU
            0x28 => op.cpu.write::<u8>(op.ea(), op.rt32() as u8),    // SB
            0x29 => op.cpu.write::<u16>(op.ea(), op.rt32() as u16),  // SH
            0x2A => op.cpu.write::<u32>(op.ea(), op.cpu.swl(op.ea(), op.rt32())), // SWL
            0x2B => op.cpu.write::<u32>(op.ea(), op.rt32()),         // SW
            0x2E => op.cpu.write::<u32>(op.ea(), op.cpu.swr(op.ea(), op.rt32())), // SWR
            0x2F => {}                                               // CACHE

            0x31 => if_cop!(op, cop1, cop1.lwc(op.opcode, &op.cpu.ctx, &op.cpu.bus)), // LWC1
            0x32 => if_cop!(op, cop2, cop2.lwc(op.opcode, &op.cpu.ctx, &op.cpu.bus)), // LWC2
            0x35 => if_cop!(op, cop1, cop1.ldc(op.opcode, &op.cpu.ctx, &op.cpu.bus)), // LDC1
            0x36 => if_cop!(op, cop2, cop2.ldc(op.opcode, &op.cpu.ctx, &op.cpu.bus)), // LDC2
            0x37 => *op.mrt64() = op.cpu.read::<u64>(op.ea()),                        // LD
            0x39 => if_cop!(op, cop1, cop1.swc(op.opcode, &op.cpu.ctx, &op.cpu.bus)), // SWC1
            0x3A => if_cop!(op, cop2, cop2.swc(op.opcode, &op.cpu.ctx, &op.cpu.bus)), // SWC2
            0x3D => if_cop!(op, cop1, cop1.sdc(op.opcode, &op.cpu.ctx, &op.cpu.bus)), // SDC1
            0x3E => if_cop!(op, cop2, cop2.sdc(op.opcode, &op.cpu.ctx, &op.cpu.bus)), // SDC2
            0x3F => op.cpu.write::<u64>(op.ea(), op.rt64()),                          // SD

            _ => panic!(
                "unimplemented opcode: func=0x{:x?}, pc={}",
                op.op(),
                op.cpu.ctx.pc.hex()
            ),
        }
    }

    fn lwl(&self, addr: u32, reg: u32) -> u32 {
        let mem = self.read::<u32>(addr);
        let shift = (addr & 3) * 8;
        let mask = (1 << shift) - 1;
        (reg & mask) | ((mem << shift) & !mask)
    }

    fn lwr(&self, addr: u32, reg: u32) -> u32 {
        let mem = self.read::<u32>(addr);
        let shift = (!addr & 3) * 8;
        let mask = ((1u64 << (32 - shift)) - 1) as u32;
        (reg & !mask) | ((mem >> shift) & mask)
    }

    fn swl(&self, addr: u32, reg: u32) -> u32 {
        let mem = self.read::<u32>(addr);
        let shift = (addr & 3) * 8;
        let mask = ((1u64 << (32 - shift)) - 1) as u32;
        (mem & !mask) | ((reg >> shift) & mask)
    }

    fn swr(&self, addr: u32, reg: u32) -> u32 {
        let mem = self.read::<u32>(addr);
        let shift = (!addr & 3) * 8;
        let mask = (1 << shift) - 1;
        (mem & mask) | ((reg << shift) & !mask)
    }

    fn fetch(&mut self, addr: u32) -> &MemIoR<u32> {
        // Save last fetched memio, to speed up hot loops
        if self.last_fetch_addr != addr {
            self.last_fetch_addr = addr;
            self.last_fetch_mem = self.bus.borrow().fetch_read::<u32>(addr & 0x1FFF_FFFC);
        }
        &self.last_fetch_mem
    }

    fn read<U: MemInt>(&self, addr: u32) -> U {
        self.bus
            .borrow()
            .read::<U>(addr & 0x1FFF_FFFF & !(U::SIZE as u32 - 1))
    }

    fn write<U: MemInt>(&self, addr: u32, val: U) {
        self.bus
            .borrow()
            .write::<U>(addr & 0x1FFF_FFFF & !(U::SIZE as u32 - 1), val);
    }

    pub fn run(&mut self, until: i64) {
        self.until = until;
        while self.ctx.clock < self.until {
            // TODO: check lines
            // if self.ctx.lines.halt {
            //     self.ctx.clock = self.until;
            //     return;
            // }

            if let Some(ref mut cop0) = self.cop0 {
                if cop0.pending_int() {
                    cop0.exception(&mut self.ctx, Exception::INT);
                    continue;
                }
            }

            let pc = self.ctx.pc;
            let mut iter = self.fetch(pc).iter().unwrap();

            // Tight loop: go through continuous memory, no branches, no IRQs
            self.ctx.tight_exit = false;
            while let Some(op) = iter.next() {
                self.ctx.pc += 4;
                self.op(op);
                if self.ctx.clock >= self.until || self.ctx.tight_exit {
                    break;
                }
            }

            if self.ctx.branch_pc != 0 {
                let pc = self.ctx.pc;
                let op = iter.next().unwrap_or_else(|| self.fetch(pc).read());
                self.ctx.pc = self.ctx.branch_pc;
                self.ctx.branch_pc = 0;
                self.op(op);
            }
        }
    }
}

/*
pub trait Executor<'a, 'c: 'a> {
    type Ctx;

    fn exec_begin(&'a mut self) -> (&'a mut Self, Self::Ctx);
    fn exec_step(&mut self, ectx: &mut Self::Ctx) -> bool;
    fn exec_finish(&mut self, ectx: Self::Ctx);
}

use emu::bus::MemIoRIterator;

pub struct ExecContext<'c> {
    iter: MemIoRIterator<'c, u32>,
}

impl<'a, 'c: 'a> Executor<'a, 'c> for Box<Cpu> {
    type Ctx = ExecContext<'c>;

    #[inline(always)]
    fn exec_begin(&'a mut self) -> (&'a mut Self, ExecContext<'static>) {
        let pc = self.ctx.pc;
        let iter = self
            .bus
            .borrow()
            .fetch_read::<u32>(pc & 0x1FFF_FFFC)
            .iter()
            .unwrap();
        self.ctx.tight_exit = false;
        (self, ExecContext { iter })
    }

    #[inline(always)]
    fn exec_step(&mut self, ectx: &mut ExecContext) -> bool {
        if let Some(op) = ectx.iter.next() {
            self.ctx.pc += 4;
            self.op(op);
            return !self.ctx.tight_exit;
        }
        return false;
    }
    #[inline(always)]
    fn exec_finish(&mut self, mut ectx: ExecContext) {
        if self.ctx.branch_pc != 0 {
            let pc = self.ctx.pc;
            let op = ectx.iter.next().unwrap_or_else(|| self.fetch(pc).read());
            self.ctx.pc = self.ctx.branch_pc;
            self.ctx.branch_pc = 0;
            self.op(op);
        }
        // if let Some(ref mut cop0) = self.cop0 {
        //     if cop0.pending_int() {
        //         cop0.exception(&mut self.ctx, Exception::INT);
        //     }
        // }
    }
}

pub fn run_two<'a, 'c: 'a, E1: Executor<'a, 'c>, E2: Executor<'a, 'c>>(
    e1: &'a mut E1,
    n1: usize,
    e2: &'a mut E2,
    n2: usize,
) {
    let (e1, mut ectx1) = e1.exec_begin();
    let (e2, mut ectx2) = e2.exec_begin();

    'iloop: loop {
        for _ in 0..n1 {
            if !e1.exec_step(&mut ectx1) {
                break 'iloop;
            }
        }
        for _ in 0..n2 {
            if !e2.exec_step(&mut ectx2) {
                break 'iloop;
            }
        }
    }

    e1.exec_finish(ectx1);
    e2.exec_finish(ectx2);
}
*/

impl sync::Subsystem for Box<Cpu> {
    fn run(&mut self, until: i64) {
        Cpu::run(self, until)
    }

    fn cycles(&self) -> i64 {
        self.ctx.clock
    }
}
