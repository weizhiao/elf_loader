use crate::{
    arch::*, relocate_error, symbol::SymbolInfo, ElfDylib, RelocatedDylib, RelocatedInner, Result,
    ThreadLocal, Unwind,
};
use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use elf::abi::*;

#[allow(unused)]
struct SymDef<'temp> {
    sym: &'temp ElfSymbol,
    base: usize,
    #[cfg(feature = "tls")]
    tls: Option<usize>,
}

impl<'temp> From<SymDef<'temp>> for *const () {
    fn from(symdef: SymDef<'temp>) -> Self {
        if symdef.sym.st_info & 0xf != STT_GNU_IFUNC {
            (symdef.base + symdef.sym.st_value as usize) as _
        } else {
            let ifunc: fn() -> usize =
                unsafe { core::mem::transmute(symdef.base + symdef.sym.st_value as usize) };
            ifunc() as _
        }
    }
}

impl<T: ThreadLocal, U: Unwind> ElfDylib<T, U> {
    pub fn needed_libs(&self) -> &Vec<&str> {
        &self.needed_libs
    }

    /// Relocate dynamic library with given libraries
    pub fn relocate(self, libs: impl AsRef<[RelocatedDylib]>) -> Self {
        self.relocate_impl(libs.as_ref(), |_| None)
    }

    /// Relocate dynamic library with given libraries and a custom symbol finder function
    pub fn relocate_with<F>(self, libs: impl AsRef<[RelocatedDylib]>, func: F) -> Self
    where
        F: Fn(&str) -> Option<*const ()> + 'static,
    {
        let mut lib = self.relocate_impl(libs.as_ref(), |name| func(name));
        lib.closures.push(Box::new(func));
        lib
    }

    fn relocate_impl<F>(mut self, libs: &[RelocatedDylib], find_symbol: F) -> Self
    where
        F: Fn(&str) -> Option<*const ()>,
    {
        let mut relocation = core::mem::take(&mut self.relocation);

        fn find_symdef<'a, T: ThreadLocal, U: Unwind>(
            elf_lib: &ElfDylib<T, U>,
            libs: &'a [RelocatedDylib],
            dynsym: &'a ElfSymbol,
            syminfo: SymbolInfo<'_>,
        ) -> Option<SymDef<'a>> {
            if dynsym.st_shndx != SHN_UNDEF {
                Some(SymDef {
                    sym: dynsym,
                    base: elf_lib.segments.base(),
                    #[cfg(feature = "tls")]
                    tls: elf_lib.tls.as_ref().map(|tls| unsafe { tls.module_id() }),
                })
            } else {
                let mut symbol = None;
                for lib in libs.iter() {
                    if let Some(sym) = lib.inner.symbols.get_sym(&syminfo) {
                        symbol = Some(SymDef {
                            sym,
                            base: lib.base(),
                            #[cfg(feature = "tls")]
                            tls: lib.inner.tls,
                        });
                        break;
                    }
                }
                symbol
            }
        }

        /*
            A Represents the addend used to compute the value of the relocatable field.
            B Represents the base address at which a shared object has been loaded into memory during execution.
            S Represents the value of the symbol whose index resides in the relocation entry.
        */

        // 开启lazy bind后会跳过plt相关的重定位
        if !self.lazy {
            if let Some(rela_array) = &mut relocation.pltrel {
                rela_array.relocate(|rela, idx, bitmap, deal_fail| {
                    let r_type = rela.r_type();
                    let r_sym = rela.r_symbol();
                    assert!(r_sym != 0);
                    let (dynsym, syminfo) = self.symbols.rel_symbol(r_sym);
                    let symbol = if let Some(symbol) = find_symbol(syminfo.name)
                        .or(find_symdef(&self, libs, dynsym, syminfo).map(|symdef| symdef.into()))
                    {
                        symbol
                    } else {
                        deal_fail(idx, bitmap);
                        return;
                    };
                    match r_type as _ {
                        // S
                        // 对于.rela.plt来说通常只有这一种重定位类型
                        REL_JUMP_SLOT => {
                            self.write_val(rela.r_offset(), symbol as usize);
                        }
                        _ => {
                            unreachable!()
                        }
                    }
                });
            }
        }

        if let Some(rela_array) = &mut relocation.dynrel {
            rela_array.relocate(|rela, idx, bitmap, deal_fail| {
                let r_type = rela.r_type();
                let r_sym = rela.r_symbol();
                let mut name = "";
                let symdef = if r_sym != 0 {
                    let (dynsym, syminfo) = self.symbols.rel_symbol(r_sym);
                    name = syminfo.name;
                    find_symdef(&self, libs, dynsym, syminfo)
                } else {
                    None
                };

                match r_type as _ {
                    // REL_GOT: S  REL_SYMBOLIC: S + A
                    REL_GOT | REL_SYMBOLIC => {
                        let symbol = if let Some(symbol) =
                            find_symbol(name).or(symdef.map(|symdef| symdef.into()))
                        {
                            symbol
                        } else {
                            deal_fail(idx, bitmap);
                            return;
                        };
                        self.write_val(rela.r_offset(), symbol as usize + rela.r_addend());
                    }
                    // B + A
                    REL_RELATIVE => {
                        self.write_val(rela.r_offset(), self.segments.base() + rela.r_addend());
                    }
                    // ELFTLS
                    #[cfg(feature = "tls")]
                    REL_DTPMOD => {
                        if r_sym != 0 {
                            let symdef = if let Some(symdef) = symdef {
                                symdef
                            } else {
                                deal_fail(idx, bitmap);
                                return;
                            };
                            self.write_val(rela.r_offset(), symdef.tls.unwrap());
                        } else {
                            self.write_val(rela.r_offset(), unsafe {
                                self.tls.as_ref().unwrap().module_id()
                            });
                        }
                    }
                    #[cfg(feature = "tls")]
                    REL_DTPOFF => {
                        let symdef = if let Some(symdef) = symdef {
                            symdef
                        } else {
                            deal_fail(idx, bitmap);
                            return;
                        };
                        // offset in tls
                        let tls_val = (symdef.sym.st_value as usize + rela.r_addend())
                            .wrapping_sub(TLS_DTV_OFFSET);
                        self.write_val(rela.r_offset(), tls_val);
                    }
                    _ => unimplemented!(),
                }
            });
        }

        self.relocation = relocation;
        self.dep_libs.extend_from_slice(libs);
        self
    }

    #[inline(always)]
    fn write_val(&self, offset: usize, val: usize) {
        unsafe {
            let rel_addr = (self.segments.base() + offset) as *mut usize;
            rel_addr.write(val)
        };
    }

    /// Whether there are any items that have not been relocated
    #[inline]
    pub fn is_finished(&self) -> bool {
        let mut finished = true;
        if !self.lazy {
            if let Some(array) = &self.relocation.pltrel {
                finished = array.is_finished();
            }
        }
        if let Some(array) = &self.relocation.dynrel {
            finished = array.is_finished();
        }
        finished
    }

    /// Finish relocation
    pub fn finish(mut self) -> Result<RelocatedDylib> {
        if !self.is_finished() {
            return Err(relocate_error(self.not_relocated()));
        }
        if let Some(init) = self.init_fn {
            init();
        }
        if let Some(init_array) = self.init_array_fn {
            for init in init_array {
                init();
            }
        }
        if !self.lazy {
            if let Some(relro) = self.relro {
                relro.relro()?;
            }
        }
        #[cfg(feature = "tls")]
        let tls = self.tls.map(|t| {
            let tls = unsafe { t.module_id() };
            self.user_data.data_mut().push(Box::new(t));
            tls
        });
        if let Some(u) = self.unwind {
            self.user_data.data_mut().push(Box::new(u));
        }

        Ok(RelocatedDylib {
            inner: Arc::new(RelocatedInner {
                name: self.name,
                symbols: self.symbols,
                dynamic: self.dynamic,
                pltrel: self
                    .relocation
                    .pltrel
                    .map(|array| array.array.as_ptr())
                    .unwrap_or(core::ptr::null()),
                #[cfg(feature = "tls")]
                tls,
                segments: self.segments,
                fini_fn: self.fini_fn,
                fini_array_fn: self.fini_array_fn,
                user_data: self.user_data,
                dep_libs: self.dep_libs.into_boxed_slice(),
                closures: self.closures.into_boxed_slice(),
            }),
        })
    }

    #[cold]
    #[inline(never)]
    fn not_relocated(&mut self) -> String {
        let mut f = String::new();
        f.push_str(&format!(
            "{}: The symbols that have not been relocated:   ",
            self.name.to_str().unwrap()
        ));
        if let Some(array) = &mut self.relocation.pltrel {
            let mut iter = BitMapIterator::new(&mut array.state);
            while let Some((_, idx)) = iter.next() {
                let rela = &array.array[idx];
                let r_sym = rela.r_symbol();
                if r_sym != 0 {
                    let (_, syminfo) = self.symbols.rel_symbol(r_sym);
                    f.push_str(&format!("[{}] ", syminfo.name));
                }
            }
        }
        if let Some(array) = &mut self.relocation.dynrel {
            let mut iter = BitMapIterator::new(&mut array.state);
            while let Some((_, idx)) = iter.next() {
                let rela = &array.array[idx];
                let r_sym = rela.r_symbol();
                if r_sym != 0 {
                    let (_, syminfo) = self.symbols.rel_symbol(r_sym);
                    f.push_str(&format!("[{}] ", syminfo.name));
                }
            }
        }
        f
    }
}

#[derive(Default)]
pub(crate) struct ElfRelocation {
    pltrel: Option<ElfRelaArray>,
    dynrel: Option<ElfRelaArray>,
}

impl ElfRelocation {
    #[inline]
    pub(crate) fn new(
        pltrel: Option<&'static [ElfRela]>,
        dynrel: Option<&'static [ElfRela]>,
    ) -> Self {
        let pltrel = pltrel.map(|array| ElfRelaArray {
            array,
            state: RelocateState {
                relocated: BitMap::new(array.len()),
                stage: RelocateStage::Init,
            },
        });
        let dynrel = dynrel.map(|array| ElfRelaArray {
            array,
            state: RelocateState {
                relocated: BitMap::new(array.len()),
                stage: RelocateStage::Init,
            },
        });
        Self { pltrel, dynrel }
    }
}

#[derive(PartialEq, Eq)]
enum RelocateStage {
    Init,
    Relocating,
    Finish,
}

struct RelocateState {
    // 位图用于记录对应的项是否已经被重定位，已经重定位的项对应的bit会设为1
    relocated: BitMap,
    stage: RelocateStage,
}

struct ElfRelaArray {
    array: &'static [ElfRela],
    state: RelocateState,
}

struct BitMapIterator<'bitmap> {
    cur_bit: u32,
    index: usize,
    state: &'bitmap mut RelocateState,
}

impl<'bitmap> BitMapIterator<'bitmap> {
    fn new(state: &'bitmap mut RelocateState) -> Self {
        Self {
            cur_bit: state.relocated.unit(0),
            index: 0,
            state,
        }
    }

    fn next(&mut self) -> Option<(&mut RelocateState, usize)> {
        loop {
            let idx = self.cur_bit.trailing_ones();
            if idx == 32 {
                self.index += 1;
                if self.index == self.state.relocated.unit_count() {
                    break None;
                }
                self.cur_bit = self.state.relocated.unit(self.index);
            } else {
                self.cur_bit |= 1 << idx;
                break Some((self.state, self.index * 32 + idx as usize));
            }
        }
    }
}

impl ElfRelaArray {
    #[inline]
    fn is_finished(&self) -> bool {
        if self.state.stage != RelocateStage::Finish {
            return false;
        }
        true
    }

    fn relocate(
        &mut self,
        f: impl Fn(&ElfRela, usize, &mut RelocateState, fn(usize, &mut RelocateState)),
    ) {
        match self.state.stage {
            RelocateStage::Init => {
                let deal_fail = |idx: usize, state: &mut RelocateState| {
                    state.relocated.clear(idx);
                    state.stage = RelocateStage::Relocating;
                };
                self.state.stage = RelocateStage::Finish;
                for (idx, rela) in self.array.iter().enumerate() {
                    f(rela, idx, &mut self.state, deal_fail);
                }
            }
            RelocateStage::Relocating => {
                let deal_fail = |idx: usize, state: &mut RelocateState| {
                    // 重定位失败
                    state.relocated.clear(idx);
                    state.stage = RelocateStage::Relocating;
                };
                self.state.stage = RelocateStage::Finish;
                let mut iter = BitMapIterator::new(&mut self.state);
                while let Some((state, idx)) = iter.next() {
                    state.relocated.set(idx);
                    f(&self.array[idx], idx, state, deal_fail);
                }
            }
            RelocateStage::Finish => {}
        }
    }
}

struct BitMap {
    bitmap: Vec<u32>,
}

impl BitMap {
    #[inline]
    fn new(size: usize) -> Self {
        let bitmap_size = (size + 31) / 32;
        let mut bitmap = Vec::new();
        // 初始时全部标记为已重定位
        bitmap.resize(bitmap_size, u32::MAX);
        Self { bitmap }
    }

    #[inline]
    fn unit(&self, index: usize) -> u32 {
        self.bitmap[index]
    }

    #[inline]
    fn unit_count(&self) -> usize {
        self.bitmap.len()
    }

    #[inline]
    fn set(&mut self, bit_index: usize) {
        let unit_index = bit_index / 32;
        let index = bit_index % 32;
        self.bitmap[unit_index] |= 1 << index;
    }

    #[inline]
    fn clear(&mut self, bit_index: usize) {
        let unit_index = bit_index / 32;
        let index = bit_index % 32;
        self.bitmap[unit_index] &= !(1 << index);
    }
}
