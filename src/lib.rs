//! # elf_loader
//! A `lightweight`, `extensible`, and `high-performance` library for loading ELF files.
//! ## Usage
//! It implements the general steps for loading ELF files and leaves extension interfaces,
//! allowing users to implement their own customized loaders.
//! ## Example
//! This repository provides an example of a [mini-loader](https://github.com/weizhiao/elf_loader/tree/main/mini-loader) implemented using `elf_loader`.
//! The miniloader can load PIE files and currently only supports `x86_64`.
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
)))]
compile_error!("unsupport arch");

pub mod arch;
pub mod dynamic;
mod loader;
pub mod mmap;
pub mod object;
mod relocation;
pub mod segment;
mod symbol;
#[cfg(feature = "version")]
mod version;

use alloc::{
    boxed::Box,
    ffi::CString,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use arch::{Dyn, ElfRela, Phdr};
use core::{
    any::Any,
    ffi::CStr,
    fmt::{Debug, Display},
    marker::PhantomData,
    ops::{self, Range},
};
use dynamic::ElfDynamic;

use object::*;
use relocation::ElfRelocation;
use segment::{ELFRelro, ElfSegments};
use symbol::{SymbolData, SymbolInfo};

pub use elf::abi;
pub use loader::Loader;

impl<T: ThreadLocal, U: Unwind> Debug for ElfDylib<T, U> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ELFLibrary")
            .field("name", &self.name)
            .field("needed_libs", &self.needed_libs)
            .finish()
    }
}

/// Handle the parts of the elf file related to the ehframe
pub trait Unwind: Sized + 'static {
    unsafe fn new(phdr: &Phdr, map_range: Range<usize>) -> Option<Self>;
}

/// Handles the parts of the elf file related to thread local storage
pub trait ThreadLocal: Sized + 'static {
    unsafe fn new(phdr: &Phdr, base: usize) -> Option<Self>;
    unsafe fn module_id(&self) -> usize;
}

/// User-defined data associated with the loaded ELF file
pub struct UserData {
    data: Vec<Box<dyn Any + 'static>>,
}

impl UserData {
    #[inline]
    pub const fn empty() -> Self {
        Self { data: Vec::new() }
    }

    #[inline]
    pub fn data_mut(&mut self) -> &mut Vec<Box<dyn Any + 'static>> {
        &mut self.data
    }

    #[inline]
    pub fn data(&self) -> &[Box<dyn Any>] {
        &self.data
    }
}

/// An unrelocated dynamic library
pub struct ElfDylib<T, U>
where
    T: ThreadLocal,
    U: Unwind,
{
    /// file name
    name: CString,
    /// phdr
    phdrs: &'static [Phdr],
    /// entry
    entry: usize,
    /// .got.plt
    got: Option<*mut usize>,
    /// elf symbols
    symbols: SymbolData,
    /// dynamic
    dynamic: *const Dyn,
    #[cfg(feature = "tls")]
    /// .tbss and .tdata
    tls: Option<T>,
    /// .eh_frame
    unwind: Option<U>,
    /// semgents
    segments: ElfSegments,
    /// .fini
    fini_fn: Option<extern "C" fn()>,
    /// .fini_array
    fini_array_fn: Option<&'static [extern "C" fn()]>,
    /// user data
    user_data: UserData,
    /// dependency libraries
    dep_libs: Vec<RelocatedDylib>,
    /// function closure
    closures: Vec<Box<dyn Fn(&str) -> Option<*const ()>>>,
    /// rela.dyn and rela.plt
    relocation: ElfRelocation,
    /// GNU_RELRO segment
    relro: Option<ELFRelro>,
    /// .init
    init_fn: Option<extern "C" fn()>,
    /// .init_array
    init_array_fn: Option<&'static [extern "C" fn()]>,
    /// needed libs' name
    needed_libs: Vec<&'static str>,
    /// lazy binding
    lazy: bool,
    _marker: PhantomData<T>,
}

impl<T: ThreadLocal, U: Unwind> ElfDylib<T, U> {
    /// Get the entry point of the dynamic library.
    #[inline]
    pub fn entry(&self) -> usize {
        self.entry + self.base()
    }

    /// Get phdrs of the dynamic library.
    #[inline]
    pub fn phdrs(&self) -> &[Phdr] {
        self.phdrs
    }

    /// Get the C-style name of the dynamic library.
    #[inline]
    pub fn cname(&self) -> &CStr {
        self.name.as_c_str()
    }

    /// Get the name of the dynamic library.
    #[inline]
    pub fn name(&self) -> &str {
        self.name.to_str().unwrap()
    }

    /// Gets the address of the dynamic section.
    #[inline]
    pub fn dynamic(&self) -> *const Dyn {
        self.dynamic
    }

    /// Get the base address of the dynamic library.
    #[inline]
    pub fn base(&self) -> usize {
        self.segments.base()
    }

    /// Get user-defined data.
    #[inline]
    pub unsafe fn user_data_mut(&mut self) -> &mut UserData {
        &mut self.user_data
    }

    /// Get user-defined data.
    #[inline]
    pub fn user_data(&self) -> &UserData {
        &self.user_data
    }
}

#[allow(unused)]
pub struct Dylib {
    name: CString,
    symbols: SymbolData,
    dynamic: *const Dyn,
    pltrel: *const ElfRela,
    #[cfg(feature = "tls")]
    tls: Option<usize>,
    /// .fini
    fini_fn: Option<extern "C" fn()>,
    /// .fini_array
    fini_array_fn: Option<&'static [extern "C" fn()]>,
    /// user data
    user_data: UserData,
    /// dependency libraries
    dep_libs: Box<[RelocatedDylib]>,
    /// function closure
    closures: Box<[Box<dyn Fn(&str) -> Option<*const ()>>]>,
    /// semgents
    segments: ElfSegments,
}

impl Drop for Dylib {
    fn drop(&mut self) {
        if let Some(f) = self.fini_fn {
            f();
        }

        if let Some(array) = self.fini_array_fn {
            for f in array {
                f();
            }
        }
    }
}

impl Debug for Dylib {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dylib")
            .field("name", &self.name)
            .field("dep", &self.dep_libs)
            .finish()
    }
}

impl Dylib {
    /// Get the name of the dynamic library.
    #[inline]
    pub fn name(&self) -> &str {
        self.name.to_str().unwrap()
    }

    /// Get the C-style name of the dynamic library.
    #[inline]
    pub fn cname(&self) -> &CStr {
        &self.name
    }

    /// Get the base address of the dynamic library.
    #[inline]
    pub fn base(&self) -> usize {
        self.segments.base()
    }

    /// Get the user data of the dynamic library.
    #[inline]
    pub fn user_data(&self) -> &UserData {
        &self.user_data
    }

    #[allow(unused_variables)]
    pub unsafe fn from_raw(
        name: CString,
        base: usize,
        dynamic: ElfDynamic,
        tls: Option<usize>,
        mut segments: ElfSegments,
        user_data: UserData,
    ) -> Self {
        segments.offset = (segments.memory.as_ptr() as usize).wrapping_sub(base);
        Self {
            name,
            symbols: SymbolData::new(&dynamic),
            pltrel: core::ptr::null(),
            dynamic: dynamic.dyn_ptr,
            #[cfg(feature = "tls")]
            tls,
            segments,
            fini_fn: None,
            fini_array_fn: None,
            user_data,
            dep_libs: Box::new([]),
            closures: Box::new([]),
        }
    }

    pub unsafe fn get<'lib, T>(&'lib self, name: &str) -> Result<Symbol<'lib, T>> {
        self.symbols
            .get_sym(&SymbolInfo::new(name))
            .map(|sym| Symbol {
                ptr: (self.base() + sym.st_value as usize) as _,
                pd: PhantomData,
            })
            .ok_or(find_symbol_error(format!("can not find symbol:{}", name)))
    }

    #[cfg(feature = "version")]
    pub unsafe fn get_version<'lib, T>(
        &'lib self,
        name: &str,
        version: &str,
    ) -> Result<Symbol<'lib, T>> {
        self.symbols
            .get_sym(&SymbolInfo::new_with_version(name, version))
            .map(|sym| Symbol {
                ptr: (self.base() + sym.st_value as usize) as _,
                pd: PhantomData,
            })
            .ok_or(find_symbol_error(format!("can not find symbol:{}", name)))
    }
}

/// A symbol from dynamic library
#[derive(Debug, Clone)]
pub struct Symbol<'lib, T: 'lib> {
    ptr: *mut (),
    pd: PhantomData<&'lib T>,
}

impl<'lib, T> ops::Deref for Symbol<'lib, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*(&self.ptr as *const *mut _ as *const T) }
    }
}

/// A dynamic library that has been relocated
#[derive(Clone)]
pub struct RelocatedDylib {
    pub inner: Arc<Dylib>,
}

impl Debug for RelocatedDylib {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.inner.fmt(f)
    }
}

unsafe impl Send for RelocatedDylib {}

impl RelocatedDylib {
    /// Get dependent libraries.
    /// # Examples
    ///
    /// ```no_run
    /// if let Some(dependencies) = library.dep_libs() {
    ///     for lib in dependencies {
    ///         println!("Dependency: {:?}", lib);
    ///     }
    /// } else {
    ///     println!("No dependencies found.");
    /// }
    /// ```
    #[inline]
    pub fn dep_libs(&self) -> Option<&[RelocatedDylib]> {
        if self.inner.dep_libs.is_empty() {
            None
        } else {
            Some(&self.inner.dep_libs)
        }
    }

    /// Get the name of the dynamic library.
    #[inline]
    pub fn name(&self) -> &str {
        self.inner.name()
    }

    /// Get the C-style name of the dynamic library.
    #[inline]
    pub fn cname(&self) -> &CStr {
        &self.inner.cname()
    }

    /// Get the base address of the dynamic library.
    #[inline]
    pub fn base(&self) -> usize {
        self.inner.base()
    }

    /// Get the user data of the dynamic library.
    #[inline]
    pub fn user_data(&self) -> &UserData {
        &self.inner.user_data()
    }

    /// Get a pointer to a function or static variable by symbol name.
    ///
    /// The symbol is interpreted as-is; no mangling is done. This means that symbols like `x::y` are
    /// most likely invalid.
    ///
    /// # Safety
    /// Users of this API must specify the correct type of the function or variable loaded.
    ///
    /// # Examples
    /// ```no_run
    /// unsafe {
    ///     let awesome_function: Symbol<unsafe extern fn(f64) -> f64> =
    ///         lib.get("awesome_function").unwrap();
    ///     awesome_function(0.42);
    /// }
    /// ```
    /// A static variable may also be loaded and inspected:
    /// ```no_run
    /// unsafe {
    ///     let awesome_variable: Symbol<*mut f64> = lib.get("awesome_variable").unwrap();
    ///     **awesome_variable = 42.0;
    /// };
    /// ```
    #[inline]
    pub unsafe fn get<'lib, T>(&'lib self, name: &str) -> Result<Symbol<'lib, T>> {
        self.inner.get(name)
    }

    /// Load a versioned symbol from the dynamic library.
    ///
    /// # Examples
    /// ```
    /// let symbol = unsafe { lib.get_version::<fn()>>("function_name", "1.0").unwrap() };
    /// ```
    #[cfg(feature = "version")]
    #[inline]
    pub unsafe fn get_version<'lib, T>(
        &'lib self,
        name: &str,
        version: &str,
    ) -> Result<Symbol<'lib, T>> {
        self.inner.get_version(name, version)
    }
}

/// elf_loader error types
#[derive(Debug)]
pub enum Error {
    /// An error occurred while opening or reading or writing elf files.
    #[cfg(feature = "std")]
    IOError { err: std::io::Error },
    /// An error occurred while memory mapping.
    MmapError { msg: String },
    /// An error occurred during dynamic library relocation.
    RelocateError { msg: String },
    /// An error occurred while looking for a symbol.
    FindSymbolError { msg: String },
    /// An error occurred while parsing dynamic section.
    ParseDynamicError { msg: &'static str },
    /// An error occurred while parsing elf header.
    ParseEhdrError { msg: String },
}

impl Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            #[cfg(feature = "std")]
            Error::IOError { err } => write!(f, "{err}"),
            Error::MmapError { msg } => write!(f, "{msg}"),
            Error::RelocateError { msg } => write!(f, "{msg}"),
            Error::FindSymbolError { msg } => write!(f, "{msg}"),
            Error::ParseDynamicError { msg } => write!(f, "{msg}"),
            Error::ParseEhdrError { msg } => write!(f, "{msg}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::IOError { err } => Some(err),
            _ => None,
        }
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for Error {
    #[cold]
    fn from(value: std::io::Error) -> Self {
        Error::IOError { err: value }
    }
}

#[cold]
#[inline(never)]
fn relocate_error(msg: impl ToString) -> Error {
    Error::RelocateError {
        msg: msg.to_string(),
    }
}

#[cold]
#[inline(never)]
fn find_symbol_error(msg: impl ToString) -> Error {
    Error::FindSymbolError {
        msg: msg.to_string(),
    }
}

#[cold]
#[inline(never)]
fn parse_dynamic_error(msg: &'static str) -> Error {
    Error::ParseDynamicError { msg }
}

#[cold]
#[inline(never)]
fn parse_ehdr_error(msg: impl ToString) -> Error {
    Error::ParseEhdrError {
        msg: msg.to_string(),
    }
}

pub type Result<T> = core::result::Result<T, Error>;
