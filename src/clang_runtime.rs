// =============================================================================
//    Copyright (c) 2025 - 2026 Haixing Hu.
//
//    SPDX-License-Identifier: Apache-2.0
//
//    Licensed under the Apache License, Version 2.0.
// =============================================================================
//! Runtime-only libclang 18 bindings; no Clang SDK or link-time library
//! required.
use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_uint;
use std::ffi::c_void;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;

use libloading::Library;
use serde_json::Value;
use serde_json::json;

use crate::error::Error;
use crate::error::config;
use crate::native_token::NativeToken;

#[repr(C)]
#[derive(Clone, Copy)]
struct CxString {
    data: *const c_void,
    private_flags: c_uint,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Location {
    ptr_data: [*const c_void; 2],
    int_data: c_uint,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct SourceRange {
    ptr_data: [*const c_void; 2],
    begin_int_data: c_uint,
    end_int_data: c_uint,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Token {
    int_data: [c_uint; 4],
    ptr_data: *mut c_void,
}

type Index = *mut c_void;
type TranslationUnit = *mut c_void;
type File = *mut c_void;
type CreateIndex = unsafe extern "C" fn(c_int, c_int) -> Index;
type DisposeIndex = unsafe extern "C" fn(Index);
type Parse = unsafe extern "C" fn(
    Index,
    *const c_char,
    *const *const c_char,
    c_int,
    *mut c_void,
    c_uint,
    c_uint,
    *mut TranslationUnit,
) -> c_int;
type DisposeTranslationUnit = unsafe extern "C" fn(TranslationUnit);
type GetFile = unsafe extern "C" fn(TranslationUnit, *const c_char) -> File;
type GetFileContents = unsafe extern "C" fn(TranslationUnit, File, *mut usize) -> *const c_char;
type GetLocation = unsafe extern "C" fn(TranslationUnit, File, c_uint) -> Location;
type GetRange = unsafe extern "C" fn(Location, Location) -> SourceRange;
type Tokenize = unsafe extern "C" fn(TranslationUnit, SourceRange, *mut *mut Token, *mut c_uint);
type DisposeTokens = unsafe extern "C" fn(TranslationUnit, *mut Token, c_uint);
type TokenKind = unsafe extern "C" fn(Token) -> c_uint;
type TokenExtent = unsafe extern "C" fn(TranslationUnit, Token) -> SourceRange;
type RangeLocation = unsafe extern "C" fn(SourceRange) -> Location;
type SpellingLocation = unsafe extern "C" fn(Location, *mut File, *mut c_uint, *mut c_uint, *mut c_uint);
type GetVersion = unsafe extern "C" fn() -> CxString;
type GetCString = unsafe extern "C" fn(CxString) -> *const c_char;
type DisposeString = unsafe extern "C" fn(CxString);

pub(crate) struct ClangRuntime {
    _library: Library,
    info: Value,
    create_index: CreateIndex,
    dispose_index: DisposeIndex,
    parse: Parse,
    dispose_translation_unit: DisposeTranslationUnit,
    get_file: GetFile,
    get_file_contents: GetFileContents,
    get_location: GetLocation,
    get_range: GetRange,
    tokenize: Tokenize,
    dispose_tokens: DisposeTokens,
    token_kind: TokenKind,
    token_extent: TokenExtent,
    range_start: RangeLocation,
    range_end: RangeLocation,
    spelling_location: SpellingLocation,
}

impl ClangRuntime {
    pub fn load() -> Result<Self, Error> {
        let override_path = std::env::var_os("CPP_INFRA_LIBCLANG");
        let candidates = match &override_path {
            Some(path) => vec![PathBuf::from(path)],
            None => [
                "/usr/lib/llvm-18/lib/libclang.so.1",
                "libclang-18.so.18",
                "libclang-18.so.1",
                "libclang.so.18",
                "libclang.so.1",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
        };
        let mut opened = None;
        let mut failures = Vec::new();
        for candidate in candidates {
            // External LLVM runtimes are trusted in the same way as compiler executables.
            match unsafe { Library::new(&candidate) } {
                Ok(library) => {
                    opened = Some((candidate, library));
                    break;
                }
                Err(error) => failures.push(format!("{}: {error}", candidate.display())),
            }
        }
        let (candidate,library)=opened.ok_or_else(||Error::Infrastructure(format!("libclang 18 runtime unavailable; configure CPP_INFRA_LIBCLANG (no fallback for an explicit override): {}",failures.join("; "))))?;
        macro_rules! symbol {
            ($name:literal,$ty:ty) => {{
                // The C API signatures below follow clang-c/Index.h and are stable across Clang
                // 18.
                *unsafe { library.get::<$ty>(concat!($name, "\0").as_bytes()) }
                    .map_err(|error| config(format!("libclang runtime lacks {}: {error}", $name)))?
            }};
        }
        let get_version = symbol!("clang_getClangVersion", GetVersion);
        let get_cstring = symbol!("clang_getCString", GetCString);
        let dispose_string = symbol!("clang_disposeString", DisposeString);
        let version_value = unsafe { get_version() };
        let version_pointer = unsafe { get_cstring(version_value) };
        let version_bytes = if version_pointer.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(version_pointer) }.to_bytes().to_vec())
        };
        unsafe { dispose_string(version_value) };
        let version =
            String::from_utf8(version_bytes.ok_or_else(|| config("libclang version is empty"))?).map_err(config)?;
        if !version.contains("version 18.") {
            return Err(config(format!("libclang runtime must be version 18.x: {version}")));
        }
        let mut actual = candidate;
        // dladdr identifies the actual file when discovery used a soname.
        let mut details: libc::Dl_info = unsafe { std::mem::zeroed() };
        if unsafe { libc::dladdr(get_version as *const () as *const c_void, &mut details) } != 0
            && !details.dli_fname.is_null()
        {
            let path = unsafe { CStr::from_ptr(details.dli_fname) }.to_str().map_err(config)?;
            actual = PathBuf::from(path).canonicalize()?;
        }
        let info = json!({"path":actual,"version":version});
        let runtime = Self {
            info,
            create_index: symbol!("clang_createIndex", CreateIndex),
            dispose_index: symbol!("clang_disposeIndex", DisposeIndex),
            parse: symbol!("clang_parseTranslationUnit2", Parse),
            dispose_translation_unit: symbol!("clang_disposeTranslationUnit", DisposeTranslationUnit),
            get_file: symbol!("clang_getFile", GetFile),
            get_file_contents: symbol!("clang_getFileContents", GetFileContents),
            get_location: symbol!("clang_getLocationForOffset", GetLocation),
            get_range: symbol!("clang_getRange", GetRange),
            tokenize: symbol!("clang_tokenize", Tokenize),
            dispose_tokens: symbol!("clang_disposeTokens", DisposeTokens),
            token_kind: symbol!("clang_getTokenKind", TokenKind),
            token_extent: symbol!("clang_getTokenExtent", TokenExtent),
            range_start: symbol!("clang_getRangeStart", RangeLocation),
            range_end: symbol!("clang_getRangeEnd", RangeLocation),
            spelling_location: symbol!("clang_getSpellingLocation", SpellingLocation),
            _library: library,
        };
        Ok(runtime)
    }

    pub fn info(&self) -> Value {
        self.info.clone()
    }

    /// Returns raw lexical spelling ranges. Semantic diagnostics are handled
    /// only by the verified build and clang-tidy, not by this
    /// classification helper.
    pub fn tokens(&self, path: &Path, source: &str) -> Result<Vec<NativeToken>, Error> {
        let filename =
            CString::new(path.to_str().ok_or_else(|| config("lexer input path must be UTF-8"))?).map_err(config)?;
        if source.len() > u32::MAX as usize {
            return Err(config("lexer input is too large for Clang source offsets"));
        }
        let mut handles = Handles {
            runtime: self,
            index: unsafe { (self.create_index)(0, 0) },
            tu: ptr::null_mut(),
            tokens: ptr::null_mut(),
            count: 0,
        };
        if handles.index.is_null() {
            return Err(Error::Infrastructure("libclang could not create an index".into()));
        }
        let args = [
            c"-x".as_ptr(),
            c"c++".as_ptr(),
            c"-std=c++20".as_ptr(),
            c"-Wno-everything".as_ptr(),
        ];
        // Keep going after semantic/include errors, skip function-body parsing,
        // and tokenize the complete original file independently of those errors.
        let status = unsafe {
            (self.parse)(
                handles.index,
                filename.as_ptr(),
                args.as_ptr(),
                args.len() as c_int,
                ptr::null_mut(),
                0,
                0x242,
                &mut handles.tu,
            )
        };
        if status != 0 || handles.tu.is_null() {
            return Err(config(format!(
                "libclang could not classify {} (code {status})",
                path.display()
            )));
        }
        let file = unsafe { (self.get_file)(handles.tu, filename.as_ptr()) };
        if file.is_null() {
            return Err(config(format!("libclang did not load {}", path.display())));
        }
        let mut length = 0;
        let contents = unsafe { (self.get_file_contents)(handles.tu, file, &mut length) };
        if contents.is_null() || length != source.len() {
            return Err(config("lexer input changed while loading"));
        }
        let loaded = unsafe { std::slice::from_raw_parts(contents.cast::<u8>(), length) };
        if loaded != source.as_bytes() {
            return Err(config("lexer input changed while loading"));
        }
        let start = unsafe { (self.get_location)(handles.tu, file, 0) };
        let end = unsafe { (self.get_location)(handles.tu, file, length as c_uint) };
        let range = unsafe { (self.get_range)(start, end) };
        unsafe { (self.tokenize)(handles.tu, range, &mut handles.tokens, &mut handles.count) };
        if handles.count > 0 && handles.tokens.is_null() {
            return Err(Error::Infrastructure("libclang returned no token storage".into()));
        }
        let mut result = Vec::new();
        let mut previous_end = 0;
        for index in 0..handles.count as usize {
            let token = unsafe { *handles.tokens.add(index) };
            let extent = unsafe { (self.token_extent)(handles.tu, token) };
            let start = self.offset(unsafe { (self.range_start)(extent) }, file)?;
            let end = self.offset(unsafe { (self.range_end)(extent) }, file)?;
            let kind = unsafe { (self.token_kind)(token) };
            if start < previous_end
                || end < start
                || end > source.len()
                || kind > 4
                || !source.is_char_boundary(start)
                || !source.is_char_boundary(end)
            {
                return Err(config("libclang returned inconsistent lexical ranges"));
            }
            previous_end = end;
            result.push(NativeToken { kind, start, end });
        }
        Ok(result)
    }

    fn offset(&self, location: Location, expected: File) -> Result<usize, Error> {
        let mut file = ptr::null_mut();
        let mut offset = 0;
        unsafe { (self.spelling_location)(location, &mut file, ptr::null_mut(), ptr::null_mut(), &mut offset) };
        if file != expected {
            return Err(config("libclang lexical range refers to a different file"));
        }
        Ok(offset as usize)
    }
}

struct Handles<'a> {
    runtime: &'a ClangRuntime,
    index: Index,
    tu: TranslationUnit,
    tokens: *mut Token,
    count: c_uint,
}
impl Drop for Handles<'_> {
    fn drop(&mut self) {
        // Tokens must be disposed before their translation unit; all function
        // pointers remain valid because the borrowed runtime owns the library.
        unsafe {
            if !self.tokens.is_null() {
                (self.runtime.dispose_tokens)(self.tu, self.tokens, self.count);
            }
            if !self.tu.is_null() {
                (self.runtime.dispose_translation_unit)(self.tu);
            }
            if !self.index.is_null() {
                (self.runtime.dispose_index)(self.index);
            }
        }
    }
}
