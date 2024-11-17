#![no_std]
#![no_main]

use core::{
    arch::global_asm,
    ffi::CStr,
    panic::PanicInfo,
    ptr::{addr_of_mut, null},
};
use elf_loader::{
    abi::{DT_NULL, DT_RELA, DT_RELACOUNT, PT_DYNAMIC, PT_INTERP},
    arch::{Dyn, ElfRela, Phdr, REL_RELATIVE},
    Loader,
};
use linked_list_allocator::LockedHeap;
use mini_loader::{println, syscall::exit, MmapImpl, MyFile, MyThreadLocal, MyUnwind};
use syscalls::{syscall3, Sysno};

const AT_NULL: usize = 0;
const AT_PHDR: usize = 3;
const AT_PHENT: usize = 4;
const AT_PHNUM: usize = 5;
const AT_BASE: usize = 7;
const AT_ENTRY: usize = 9;
const AT_EXECFN: usize = 31;

#[global_allocator]
static mut ALLOCATOR: LockedHeap = LockedHeap::empty();

const HAEP_SIZE: usize = 4096;
pub static mut HEAP_BUF: [u8; HAEP_SIZE] = [0; HAEP_SIZE];

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let location = info.location().unwrap();
    println!(
        "{}:{}:{}   panic: {}",
        location.file(),
        location.line(),
        location.column(),
        info.message()
    );
    exit(-1);
}

global_asm!(include_str!("start.S"));
global_asm!(include_str!("trampoline.S"));

#[repr(C)]
struct Aux {
    tag: usize,
    val: usize,
}

// auxv <---sp + argc + 2 + env_count + 2
// 0    <---sp + argc + 2 + env_count + 1
// env  <---sp + argc + 2
// 0    <---sp + argc + 1
// argv <---sp + 1
// argc <---sp
#[no_mangle]
unsafe extern "C" fn rust_main(sp: *mut usize, dynv: *mut Dyn) {
    let argc = sp.read();
    let argv = sp.add(1);
    let env = sp.add(argc + 1 + 1);
    let mut env_count = 0;
    let mut cur_env = env;
    while cur_env.read() != 0 {
        env_count += 1;
        cur_env = cur_env.add(1);
    }
    let auxv = env.add(env_count + 1).cast::<Aux>();

    let mut cur_dyn_ptr = dynv;
    let mut cur_dyn = &*dynv;
    let mut rela = None;
    let mut rela_count = None;
    loop {
        match cur_dyn.d_tag {
            DT_NULL => break,
            DT_RELA => rela = Some(cur_dyn.d_un),
            DT_RELACOUNT => rela_count = Some(cur_dyn.d_un),
            _ => {}
        }
        cur_dyn_ptr = cur_dyn_ptr.add(1);
        cur_dyn = &mut *cur_dyn_ptr;
    }
    let rela = rela.unwrap();
    let rela_count = rela_count.unwrap();

    let mut base = 0;
    let mut phnum = 0;
    let mut ph = null();
    let mut cur_aux_ptr = auxv;
    let mut cur_aux = cur_aux_ptr.read();
    loop {
        match cur_aux.tag {
            AT_NULL => break,
            AT_PHDR => ph = cur_aux.val as *const Phdr,
            AT_PHNUM => phnum = cur_aux.val,
            AT_BASE => base = cur_aux.val,
            _ => {}
        }
        cur_aux_ptr = cur_aux_ptr.add(1);
        cur_aux = cur_aux_ptr.read();
    }

    // 通常是0，需要自行计算
    if base == 0 {
        let phdrs = core::slice::from_raw_parts(ph, phnum);
        for phdr in phdrs {
            if phdr.p_type == PT_DYNAMIC {
                base = dynv as usize - phdr.p_vaddr as usize;
                break;
            }
        }
    }
    // 自举，mini-loader自己对自己重定位
    let rela_ptr = (rela as usize + base) as *const ElfRela;
    let relas = core::slice::from_raw_parts(rela_ptr, rela_count as usize);
    for rela in relas {
        if rela.r_type() != REL_RELATIVE as usize {
            print_str("unknown rela type");
        }
        let ptr = (rela.r_offset() + base) as *mut usize;
        ptr.write(base + rela.r_append());
    }
    // 至此就完成自举，可以进行函数调用了
    ALLOCATOR = LockedHeap::new(addr_of_mut!(HEAP_BUF).cast(), HAEP_SIZE);
    if argc == 1 {
        panic!("no input file");
    }

    let elf_name = CStr::from_ptr(argv.add(1).read() as _);
    let elf_file = MyFile::new(elf_name);
    let loader = Loader::<_, MmapImpl>::new(elf_file);
    let dylib = loader.load_dylib::<MyThreadLocal, MyUnwind>().unwrap();
    let phdrs = dylib.phdrs();
    let mut interp_dylib = None;
    for phdr in phdrs {
        // 加载动态加载器ld.so
        if phdr.p_type == PT_INTERP {
            let interp_name = CStr::from_ptr((dylib.base() + phdr.p_vaddr as usize) as _);
            let interp_file = MyFile::new(interp_name);
            let interp_loader = Loader::<_, MmapImpl>::new(interp_file);
            interp_dylib = Some(
                interp_loader
                    .load_dylib::<MyThreadLocal, MyUnwind>()
                    .unwrap(),
            );
            break;
        }
    }

    let mut cur_aux_ptr = auxv as *mut Aux;
    let mut cur_aux = &mut *cur_aux_ptr;
    loop {
        match cur_aux.tag {
            AT_NULL => break,
            AT_PHDR => cur_aux.val = phdrs.as_ptr() as usize,
            AT_PHNUM => cur_aux.val = phdrs.len(),
            AT_PHENT => cur_aux.val = size_of::<Phdr>(),
            AT_ENTRY => cur_aux.val = dylib.entry(),
            AT_EXECFN => cur_aux.val = argv.add(1).read(),
            AT_BASE => cur_aux.val = 0,
            _ => {}
        }
        cur_aux_ptr = cur_aux_ptr.add(1);
        cur_aux = &mut *cur_aux_ptr;
    }

    extern "C" {
        fn trampoline(entry: usize, sp: *const usize) -> !;
    }

    // 修改argv，将mini-loader去除，这里涉及到16字节对齐，因此只能拷贝
    let size = cur_aux_ptr.add(1) as usize - sp.add(1) as usize;
    core::ptr::copy(sp.add(1), sp, size / size_of::<usize>());
    sp.write(argc - 1);

    if let Some(interp_dylib) = interp_dylib {
        trampoline(interp_dylib.entry(), sp);
    } else {
        trampoline(dylib.entry(), sp);
    }
}

pub fn print_str(s: &str) {
    let _ = unsafe { syscall3(Sysno::write, 1, s.as_ptr() as usize, s.len()) }.unwrap();
}