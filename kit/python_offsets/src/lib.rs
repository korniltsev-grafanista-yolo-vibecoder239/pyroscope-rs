use std::io::BufRead;

use object::{Object, ObjectSymbol};

#[derive(Debug, PartialEq)]
pub enum InitError {
    KindasafeInitFailed,
    PythonNotFound,
    /// `_PyRuntime` or `Py_Version` symbol not found in the ELF dynamic symbol table.
    /// Corresponds to init error code 3.
    SymbolNotFound,
    /// The ELF file could not be parsed.
    ElfParse,
    /// Failed to open or mmap the binary file.
    Io,
}

/// Absolute runtime addresses of two key CPython symbols, after applying ASLR load bias.
#[derive(Debug, PartialEq)]
pub struct ElfSymbols {
    pub py_runtime_addr: u64,
    pub py_version_addr: u64,
}

/// Open and mmap `binary.path`, parse the ELF dynamic symbol table, find
/// `_PyRuntime` and `Py_Version`, apply the ASLR load bias, and return their
/// absolute runtime addresses.
///
/// Returns [`InitError::SymbolNotFound`] (error code 3) if either symbol is absent.
pub fn resolve_elf_symbols(binary: &PythonBinary) -> Result<ElfSymbols, InitError> {
    let file = std::fs::File::open(&binary.path).map_err(|_| InitError::Io)?;
    // SAFETY: the file is a read-only view of an on-disk ELF; no other code
    // modifies it during parsing.
    let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|_| InitError::Io)?;
    resolve_elf_symbols_from_bytes(&mmap, binary.base)
}

/// Parse ELF dynamic symbols from a byte slice and compute absolute addresses.
///
/// `mapped_base` is the address at which the first mapping of this binary
/// appears in `/proc/self/maps` (i.e. the runtime base after ASLR).
fn resolve_elf_symbols_from_bytes(data: &[u8], mapped_base: u64) -> Result<ElfSymbols, InitError> {
    let obj = object::File::parse(data).map_err(|_| InitError::ElfParse)?;

    // load_bias = runtime base − ELF-file base (first LOAD segment vaddr).
    // For PIE/shared objects p_vaddr is 0, so load_bias == mapped_base.
    let load_bias = mapped_base.wrapping_sub(obj.relative_address_base());

    let mut py_runtime: Option<u64> = None;
    let mut py_version: Option<u64> = None;

    for sym in obj.dynamic_symbols() {
        match sym.name() {
            Ok("_PyRuntime") => py_runtime = Some(sym.address().wrapping_add(load_bias)),
            Ok("Py_Version") => py_version = Some(sym.address().wrapping_add(load_bias)),
            _ => {}
        }
        if py_runtime.is_some() && py_version.is_some() {
            break;
        }
    }

    match (py_runtime, py_version) {
        (Some(py_runtime_addr), Some(py_version_addr)) => Ok(ElfSymbols {
            py_runtime_addr,
            py_version_addr,
        }),
        _ => Err(InitError::SymbolNotFound),
    }
}

#[derive(Debug, PartialEq)]
pub struct PythonBinary {
    pub base: u64,
    pub path: String,
}

// Flags bitmask for /proc/maps permissions field
pub const FLAGS_READ: u32 = 0x1;
pub const FLAGS_WRITE: u32 = 0x2;
pub const FLAGS_EXEC: u32 = 0x4;
pub const FLAGS_SHARED: u32 = 0x8; // 's' = shared, 'p' = private (0)

/// Fields parsed from a single `/proc/maps` line, in order.
/// `path` is a subslice of the original line — no allocation.
type MapsLineFields<'a> = (u64, u64, u32, u64, u32, u32, u64, &'a [u8]);

/// Parse a single `/proc/maps` line.
///
/// Returns `(start, end, flags, offset, dev_major, dev_minor, inode, path_bytes)`.
/// `path_bytes` is a subslice of `line` — no allocation.
/// Returns `None` if the line is malformed.
fn parse_maps_line(line: &[u8]) -> Option<MapsLineFields<'_>> {
    // Format: start-end perms offset dev inode [path]
    // Example: 7f1234560000-7f1234580000 r--p 00000000 08:01 123456 /usr/lib/libpython3.11.so.1.0

    let mut iter = line.splitn(6, |&b| b == b' ');

    // Field 1: "start-end"
    let addr_field = iter.next()?;
    let dash = addr_field.iter().position(|&b| b == b'-')?;
    let start = u64::from_str_radix(core::str::from_utf8(&addr_field[..dash]).ok()?, 16).ok()?;
    let end = u64::from_str_radix(core::str::from_utf8(&addr_field[dash + 1..]).ok()?, 16).ok()?;

    // Field 2: "rwxp" or "rwxs"
    let perms = iter.next()?;
    if perms.len() < 4 {
        return None;
    }
    let mut flags: u32 = 0;
    if perms[0] == b'r' {
        flags |= FLAGS_READ;
    }
    if perms[1] == b'w' {
        flags |= FLAGS_WRITE;
    }
    if perms[2] == b'x' {
        flags |= FLAGS_EXEC;
    }
    if perms[3] == b's' {
        flags |= FLAGS_SHARED;
    }

    // Field 3: offset (hex)
    let offset_field = iter.next()?;
    let offset = u64::from_str_radix(core::str::from_utf8(offset_field).ok()?, 16).ok()?;

    // Field 4: "major:minor"
    let dev_field = iter.next()?;
    let colon = dev_field.iter().position(|&b| b == b':')?;
    let dev_major =
        u32::from_str_radix(core::str::from_utf8(&dev_field[..colon]).ok()?, 16).ok()?;
    let dev_minor =
        u32::from_str_radix(core::str::from_utf8(&dev_field[colon + 1..]).ok()?, 16).ok()?;

    // Field 5: inode (decimal)
    let inode_field = iter.next()?;
    let inode = core::str::from_utf8(inode_field)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;

    // Field 6: optional path (remainder), strip leading spaces and trailing newline
    let path_bytes = iter.next().map_or(b"".as_slice(), |rest| rest.trim_ascii());

    Some((
        start, end, flags, offset, dev_major, dev_minor, inode, path_bytes,
    ))
}

fn find_python_in_maps_reader<R: BufRead>(mut reader: R) -> Result<PythonBinary, InitError> {
    // We track the *first* mapping seen for each candidate.
    // libpython3 is preferred over python3.
    let mut libpython3: Option<PythonBinary> = None;
    let mut python3: Option<PythonBinary> = None;

    // Reuse a single buffer across all lines to avoid repeated allocations.
    let mut buf: Vec<u8> = Vec::with_capacity(256);

    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .map_err(|_| InitError::PythonNotFound)?;
        if n == 0 {
            break;
        }

        let (start, _end, _flags, _offset, _dev_major, _dev_minor, _inode, path_bytes) =
            match parse_maps_line(&buf) {
                Some(e) => e,
                None => continue,
            };

        // Check for libpython3 (preferred)
        if libpython3.is_none() && path_contains(path_bytes, b"libpython3") {
            libpython3 = Some(PythonBinary {
                base: start,
                path: String::from_utf8_lossy(path_bytes).into_owned(),
            });
            // Once we have a libpython3 candidate we're done — it will always win.
            break;
        }

        // Check for python3 (fallback) — only if no python3 yet
        if python3.is_none() && path_contains(path_bytes, b"python3") {
            python3 = Some(PythonBinary {
                base: start,
                path: String::from_utf8_lossy(path_bytes).into_owned(),
            });
            // Don't break here: a later libpython3 entry would be preferred.
        }
    }

    libpython3.or(python3).ok_or(InitError::PythonNotFound)
}

/// Check whether `haystack` contains the byte-string `needle` as a substring.
/// No allocation.
fn path_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Parse `/proc/self/maps` and return the `PythonBinary` describing where Python
/// (or libpython3) is loaded.
///
/// Prefers a `libpython3` mapping over a bare `python3` mapping.
/// Returns [`InitError::PythonNotFound`] (error code 2) when neither is found.
pub fn find_python_in_maps() -> Result<PythonBinary, InitError> {
    let f = std::fs::File::open("/proc/self/maps").map_err(|_| InitError::PythonNotFound)?;
    find_python_in_maps_reader(std::io::BufReader::new(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_maps_line tests ────────────────────────────────────────────────

    #[test]
    fn parse_libpython3_ro_header() {
        let line =
            b"7f1234560000-7f1234580000 r--p 00000000 08:01 123456 /usr/lib/libpython3.11.so.1.0\n";
        let (start, end, flags, offset, dev_major, dev_minor, inode, path) =
            parse_maps_line(line).unwrap();
        assert_eq!(start, 0x7f1234560000);
        assert_eq!(end, 0x7f1234580000);
        assert_eq!(flags, FLAGS_READ);
        assert_eq!(offset, 0);
        assert_eq!(dev_major, 8);
        assert_eq!(dev_minor, 1);
        assert_eq!(inode, 123456);
        assert_eq!(path, b"/usr/lib/libpython3.11.so.1.0");
    }

    #[test]
    fn parse_libpython3_exec_mapping() {
        let line =
            b"7f1234580000-7f1234600000 r-xp 00020000 08:01 123456 /usr/lib/libpython3.11.so.1.0\n";
        let (start, _end, flags, offset, _dmaj, _dmin, _inode, path) =
            parse_maps_line(line).unwrap();
        assert_eq!(start, 0x7f1234580000);
        assert_eq!(flags, FLAGS_READ | FLAGS_EXEC);
        assert_eq!(offset, 0x20000);
        assert_eq!(path, b"/usr/lib/libpython3.11.so.1.0");
    }

    #[test]
    fn parse_static_python3() {
        let line = b"555555554000-5555555b2000 r--p 00000000 08:01 654321 /usr/bin/python3\n";
        let (start, _end, flags, _off, _dmaj, _dmin, inode, path) = parse_maps_line(line).unwrap();
        assert_eq!(start, 0x555555554000);
        assert_eq!(flags, FLAGS_READ);
        assert_eq!(inode, 654321);
        assert_eq!(path, b"/usr/bin/python3");
    }

    #[test]
    fn parse_anonymous_mapping() {
        let line = b"7fff12340000-7fff12360000 rw-p 00000000 00:00 0 \n";
        let (start, _end, flags, _off, dev_major, dev_minor, inode, path) =
            parse_maps_line(line).unwrap();
        assert_eq!(start, 0x7fff12340000);
        assert_eq!(flags, FLAGS_READ | FLAGS_WRITE);
        assert_eq!(dev_major, 0);
        assert_eq!(dev_minor, 0);
        assert_eq!(inode, 0);
        assert_eq!(path, b"");
    }

    #[test]
    fn parse_anonymous_mapping_no_trailing_space() {
        // Some kernels emit no trailing space for anonymous mappings
        let line = b"7fff12340000-7fff12360000 rw-p 00000000 00:00 0\n";
        let result = parse_maps_line(line);
        assert!(result.is_some());
        let (_s, _e, _f, _o, _dm, _dn, _i, path) = result.unwrap();
        assert_eq!(path, b"");
    }

    #[test]
    fn parse_vdso() {
        let line = b"7fff12370000-7fff12372000 r-xp 00000000 00:00 0 [vdso]\n";
        let (_s, _e, flags, _o, _dm, _dn, _i, path) = parse_maps_line(line).unwrap();
        assert_eq!(flags, FLAGS_READ | FLAGS_EXEC);
        assert_eq!(path, b"[vdso]");
    }

    #[test]
    fn parse_shared_mapping() {
        let line = b"7f0000000000-7f0000010000 rw-s 00000000 00:05 0 /dev/zero\n";
        let (_s, _e, flags, _o, _dm, _dn, _i, _path) = parse_maps_line(line).unwrap();
        assert_eq!(flags, FLAGS_READ | FLAGS_WRITE | FLAGS_SHARED);
    }

    #[test]
    fn parse_malformed_line_returns_none() {
        assert!(parse_maps_line(b"not a valid maps line\n").is_none());
        assert!(parse_maps_line(b"\n").is_none());
    }

    // ── find_python_in_maps_reader tests ────────────────────────────────────

    const MAPS_LIBPYTHON3_ONLY: &[u8] = b"\
7f0000000000-7f0000020000 r--p 00000000 08:01 111 /usr/lib/libpython3.11.so.1.0\n\
7f0000020000-7f0000100000 r-xp 00020000 08:01 111 /usr/lib/libpython3.11.so.1.0\n\
7fff00000000-7fff00020000 rw-p 00000000 00:00 0\n\
";

    const MAPS_PYTHON3_ONLY: &[u8] = b"\
555555554000-5555555b2000 r--p 00000000 08:01 222 /usr/bin/python3\n\
5555555b2000-555555600000 r-xp 0005e000 08:01 222 /usr/bin/python3\n\
7fff00000000-7fff00020000 rw-p 00000000 00:00 0\n\
";

    const MAPS_BOTH: &[u8] = b"\
555555554000-5555555b2000 r--p 00000000 08:01 222 /usr/bin/python3\n\
7f0000000000-7f0000020000 r--p 00000000 08:01 111 /usr/lib/libpython3.11.so.1.0\n\
7f0000020000-7f0000100000 r-xp 00020000 08:01 111 /usr/lib/libpython3.11.so.1.0\n\
";

    const MAPS_LIBPYTHON3_MULTIPLE: &[u8] = b"\
7f0000000000-7f0000020000 r--p 00000000 08:01 111 /usr/lib/libpython3.11.so.1.0\n\
7f0000020000-7f0000100000 r-xp 00020000 08:01 111 /usr/lib/libpython3.11.so.1.0\n\
7f0000200000-7f0000210000 r--p 00000000 08:01 111 /usr/lib/libpython3.11.so.1.0\n\
";

    const MAPS_NO_PYTHON: &[u8] = b"\
7f0000000000-7f0000020000 r--p 00000000 08:01 333 /usr/lib/libc.so.6\n\
7fff00000000-7fff00020000 rw-p 00000000 00:00 0\n\
";

    fn run(maps: &[u8]) -> Result<PythonBinary, InitError> {
        find_python_in_maps_reader(std::io::Cursor::new(maps))
    }

    #[test]
    fn finds_libpython3_only() {
        let bin = run(MAPS_LIBPYTHON3_ONLY).unwrap();
        assert_eq!(bin.base, 0x7f0000000000);
        assert!(bin.path.contains("libpython3"));
    }

    #[test]
    fn finds_python3_only() {
        let bin = run(MAPS_PYTHON3_ONLY).unwrap();
        assert_eq!(bin.base, 0x555555554000);
        assert!(bin.path.contains("python3"));
    }

    #[test]
    fn prefers_libpython3_over_python3() {
        let bin = run(MAPS_BOTH).unwrap();
        assert!(
            bin.path.contains("libpython3"),
            "expected libpython3, got {}",
            bin.path
        );
        assert_eq!(bin.base, 0x7f0000000000);
    }

    #[test]
    fn returns_first_mapping_base() {
        // The first mapping (r--p, offset 0) should be the base, not the r-xp one.
        let bin = run(MAPS_LIBPYTHON3_MULTIPLE).unwrap();
        assert_eq!(bin.base, 0x7f0000000000);
    }

    #[test]
    fn returns_python_not_found_when_absent() {
        assert_eq!(run(MAPS_NO_PYTHON), Err(InitError::PythonNotFound));
    }

    #[test]
    fn empty_maps_returns_not_found() {
        assert_eq!(run(b""), Err(InitError::PythonNotFound));
    }

    #[test]
    fn python3_before_libpython3_still_prefers_libpython3() {
        // python3 entry appears first, but libpython3 comes later — must prefer libpython3
        let maps = b"\
555555554000-5555555b2000 r--p 00000000 08:01 222 /usr/bin/python3\n\
7f0000000000-7f0000020000 r--p 00000000 08:01 111 /usr/lib/libpython3.11.so.1.0\n\
";
        let bin = run(maps).unwrap();
        assert!(bin.path.contains("libpython3"), "should prefer libpython3");
    }

    // ── resolve_elf_symbols_from_bytes tests ─────────────────────────────────

    /// Build a minimal ELF64 LE shared object in memory containing exactly the
    /// symbols requested.  The file has:
    ///
    ///   ELF header  (64 B)
    ///   .dynstr     (variable, at offset 64)
    ///   .dynsym     (3 × 24 B = 72 B, follows .dynstr)
    ///   Section header table  (4 × 64 B, follows .dynsym)
    ///
    /// No segments, no PT_DYNAMIC.  The `object` crate locates `.dynsym` and
    /// `.dynstr` by section name when no PT_DYNAMIC is present, which is enough
    /// for `dynamic_symbols()` to work.
    fn build_elf_fixture(syms: &[(&str, u64)]) -> Vec<u8> {
        // ── .dynstr ──────────────────────────────────────────────────────────
        // Layout:  \0  <name0>\0  <name1>\0  …
        let mut dynstr: Vec<u8> = vec![0u8]; // index 0 = empty string (null symbol name)
        let mut name_offsets: Vec<u32> = Vec::new();
        for (name, _) in syms {
            name_offsets.push(dynstr.len() as u32);
            dynstr.extend_from_slice(name.as_bytes());
            dynstr.push(0);
        }
        // Pad .dynstr to 8-byte alignment
        while !dynstr.len().is_multiple_of(8) {
            dynstr.push(0);
        }

        // ── layout constants ─────────────────────────────────────────────────
        let elf_hdr_size: u64 = 64;
        let sym_size: u64 = 24; // Elf64_Sym
        let n_syms: u64 = 1 + syms.len() as u64; // null entry + real entries
        let shdr_size: u64 = 64; // Elf64_Shdr
        let n_shdrs: u64 = 4; // NULL, .dynstr, .dynsym, .shstrtab

        let dynstr_off = elf_hdr_size;
        let dynstr_len = dynstr.len() as u64;
        let dynsym_off = dynstr_off + dynstr_len;
        let dynsym_len = n_syms * sym_size;

        // .shstrtab: \0.dynstr\0.dynsym\0.shstrtab\0
        let shstrtab_raw: &[u8] = b"\x00.dynstr\x00.dynsym\x00.shstrtab\x00";
        let shstrtab_off = dynsym_off + dynsym_len;
        let shstrtab_len = shstrtab_raw.len() as u64;

        let shdr_off = shstrtab_off + shstrtab_len;

        // section name indices within .shstrtab
        let shstrtab_idx_dynstr: u32 = 1; // offset of ".dynstr" in shstrtab_raw
        let shstrtab_idx_dynsym: u32 = 9; // offset of ".dynsym"
        let shstrtab_idx_shstrtab: u32 = 17; // offset of ".shstrtab"

        // .shstrtab section index (for e_shstrndx)
        let shstrndx: u16 = 3;

        // ── ELF header ───────────────────────────────────────────────────────
        let mut buf: Vec<u8> = Vec::new();

        // e_ident[16]
        buf.extend_from_slice(b"\x7fELF"); // magic
        buf.push(2); // EI_CLASS = ELFCLASS64
        buf.push(1); // EI_DATA  = ELFDATA2LSB
        buf.push(1); // EI_VERSION = EV_CURRENT
        buf.push(0); // EI_OSABI = ELFOSABI_NONE
        buf.extend_from_slice(&[0u8; 8]); // padding

        buf.extend_from_slice(&3u16.to_le_bytes()); // e_type = ET_DYN
        buf.extend_from_slice(&62u16.to_le_bytes()); // e_machine = EM_X86_64
        buf.extend_from_slice(&1u32.to_le_bytes()); // e_version
        buf.extend_from_slice(&0u64.to_le_bytes()); // e_entry
        buf.extend_from_slice(&0u64.to_le_bytes()); // e_phoff (no segments)
        buf.extend_from_slice(&shdr_off.to_le_bytes()); // e_shoff
        buf.extend_from_slice(&0u32.to_le_bytes()); // e_flags
        buf.extend_from_slice(&(elf_hdr_size as u16).to_le_bytes()); // e_ehsize
        buf.extend_from_slice(&0u16.to_le_bytes()); // e_phentsize
        buf.extend_from_slice(&0u16.to_le_bytes()); // e_phnum
        buf.extend_from_slice(&(shdr_size as u16).to_le_bytes()); // e_shentsize
        buf.extend_from_slice(&(n_shdrs as u16).to_le_bytes()); // e_shnum
        buf.extend_from_slice(&shstrndx.to_le_bytes()); // e_shstrndx

        assert_eq!(buf.len(), 64);

        // ── .dynstr ──────────────────────────────────────────────────────────
        buf.extend_from_slice(&dynstr);
        assert_eq!(buf.len() as u64, dynsym_off);

        // ── .dynsym ──────────────────────────────────────────────────────────
        // Null symbol entry (all zeros)
        buf.extend_from_slice(&[0u8; 24]);

        // Real symbol entries: Elf64_Sym { st_name, st_info, st_other, st_shndx, st_value, st_size }
        for (i, (_name, addr)) in syms.iter().enumerate() {
            let st_name = name_offsets[i];
            buf.extend_from_slice(&st_name.to_le_bytes()); // st_name  (u32)
            buf.push(0x12); // st_info = STB_GLOBAL | STT_OBJECT
            buf.push(0); // st_other
            buf.extend_from_slice(&0u16.to_le_bytes()); // st_shndx = SHN_UNDEF (ok for test)
            buf.extend_from_slice(&addr.to_le_bytes()); // st_value (u64)
            buf.extend_from_slice(&0u64.to_le_bytes()); // st_size
        }

        assert_eq!(buf.len() as u64, shstrtab_off);

        // ── .shstrtab ────────────────────────────────────────────────────────
        buf.extend_from_slice(shstrtab_raw);
        assert_eq!(buf.len() as u64, shdr_off);

        // ── Section header table ─────────────────────────────────────────────
        // Helper: write one Elf64_Shdr
        let write_shdr = |buf: &mut Vec<u8>,
                          sh_name: u32,
                          sh_type: u32,
                          sh_flags: u64,
                          sh_addr: u64,
                          sh_offset: u64,
                          sh_size: u64,
                          sh_link: u32,
                          sh_info: u32,
                          sh_addralign: u64,
                          sh_entsize: u64| {
            buf.extend_from_slice(&sh_name.to_le_bytes());
            buf.extend_from_slice(&sh_type.to_le_bytes());
            buf.extend_from_slice(&sh_flags.to_le_bytes());
            buf.extend_from_slice(&sh_addr.to_le_bytes());
            buf.extend_from_slice(&sh_offset.to_le_bytes());
            buf.extend_from_slice(&sh_size.to_le_bytes());
            buf.extend_from_slice(&sh_link.to_le_bytes());
            buf.extend_from_slice(&sh_info.to_le_bytes());
            buf.extend_from_slice(&sh_addralign.to_le_bytes());
            buf.extend_from_slice(&sh_entsize.to_le_bytes());
        };

        // SHT_NULL
        write_shdr(&mut buf, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);

        // .dynstr  SHT_STRTAB = 3
        write_shdr(
            &mut buf,
            shstrtab_idx_dynstr,
            3,
            0,
            0,
            dynstr_off,
            dynstr_len,
            0,
            0,
            1,
            0,
        );

        // .dynsym  SHT_DYNSYM = 11
        // sh_link = index of .dynstr section (1), sh_info = first global symbol index (1)
        write_shdr(
            &mut buf,
            shstrtab_idx_dynsym,
            11,
            0,
            0,
            dynsym_off,
            dynsym_len,
            1,
            1,
            8,
            sym_size,
        );

        // .shstrtab  SHT_STRTAB = 3
        write_shdr(
            &mut buf,
            shstrtab_idx_shstrtab,
            3,
            0,
            0,
            shstrtab_off,
            shstrtab_len,
            0,
            0,
            1,
            0,
        );

        buf
    }

    #[test]
    fn elf_fixture_both_symbols_found() {
        let py_runtime_val: u64 = 0x1000;
        let py_version_val: u64 = 0x2000;
        let elf = build_elf_fixture(&[
            ("_PyRuntime", py_runtime_val),
            ("Py_Version", py_version_val),
        ]);

        let mapped_base: u64 = 0x7f0000000000;
        // For an ET_DYN with no PT_LOAD, relative_address_base() returns 0,
        // so load_bias = mapped_base.
        let result = resolve_elf_symbols_from_bytes(&elf, mapped_base).unwrap();
        assert_eq!(result.py_runtime_addr, mapped_base + py_runtime_val);
        assert_eq!(result.py_version_addr, mapped_base + py_version_val);
    }

    #[test]
    fn elf_fixture_missing_py_runtime() {
        let elf = build_elf_fixture(&[("Py_Version", 0x2000)]);
        let result = resolve_elf_symbols_from_bytes(&elf, 0x7f0000000000);
        assert_eq!(result, Err(InitError::SymbolNotFound));
    }

    #[test]
    fn elf_fixture_missing_py_version() {
        let elf = build_elf_fixture(&[("_PyRuntime", 0x1000)]);
        let result = resolve_elf_symbols_from_bytes(&elf, 0x7f0000000000);
        assert_eq!(result, Err(InitError::SymbolNotFound));
    }

    #[test]
    fn elf_fixture_no_symbols() {
        let elf = build_elf_fixture(&[]);
        let result = resolve_elf_symbols_from_bytes(&elf, 0x7f0000000000);
        assert_eq!(result, Err(InitError::SymbolNotFound));
    }

    #[test]
    fn elf_invalid_bytes_returns_elf_parse_error() {
        let result = resolve_elf_symbols_from_bytes(b"not an elf file", 0x1000);
        assert_eq!(result, Err(InitError::ElfParse));
    }
}
