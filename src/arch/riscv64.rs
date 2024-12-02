use elf::abi::*;

pub const EM_ARCH: u16 = EM_RISCV;
#[allow(unused)]
/* Dynamic thread vector pointers point 0x800 past the start of each
TLS block.  */
pub const TLS_DTV_OFFSET: usize = 0x800;

pub const REL_RELATIVE: u32 = R_RISCV_RELATIVE;
// RISCV does not have this
pub const REL_GOT: u32 = u32::MAX;
#[allow(unused)]
pub const REL_DTPMOD: u32 = R_RISCV_TLS_DTPMOD64;
pub const REL_SYMBOLIC: u32 = R_RISCV_64;
pub const REL_JUMP_SLOT: u32 = R_RISCV_JUMP_SLOT;
#[allow(unused)]
pub const REL_DTPOFF: u32 = R_RISCV_TLS_DTPREL64;
