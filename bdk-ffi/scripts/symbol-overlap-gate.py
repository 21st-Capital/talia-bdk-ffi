#!/usr/bin/env python3
"""TALIA FORK: the symbol-overlap gate.

Talia links several Rust static libraries into one app: the pinned libbdkffi (bdk 0.28 line),
libbitboxffi, and the new library from this branch. A linker resolves every global symbol to
ONE definition, so a name defined in two libraries means one library may run the other's code.
That is only acceptable when both definitions are the same code.

    python3 -I scripts/symbol-overlap-gate.py [--sysroot DIR] NEW.a PINNED.a [PINNED.a ...]

Each archive may be a plain ar archive or a fat (universal) file of several (lipo); archives are
compared architecture by architecture, for every architecture both have. An architecture only
one side has is listed as not compared, and the gate fails if a pinned archive shares none with
the new one, or if the two slices are built for different platforms (device against simulator).

For every global symbol the new slice defines that a pinned slice also defines, the archive
members (object files) that define it are compared, section by section, byte by byte, with every
relocation (offset, type, target); only debug sections and the version load commands (minimum OS
and SDK, reported separately) are left out. Equal members prove every symbol in them identical:
IDENTICAL. When the members differ (for example a crate's Rust object that also defines a few
C-ABI functions, such as secp256k1-sys's context functions, or the allocator shims rustc
generates into each library), the symbol's own bytes up to the next symbol and their relocations
are compared, with Rust crate disambiguators in relocation targets masked: IDENTICAL CODE.
Anything else is REVIEW, and the gate fails.

On top of that, by family:
- C and assembly (ring, secp256k1, compiler-rt): the comparison above is the whole check.
- Rust symbols (v0 mangling, `__R...`) count as the toolchain's own only when every crate root in
  the name carries a disambiguator found in the toolchain's sysroot rlibs (`--sysroot`, by
  default `rustc --print sysroot` for the toolchain rust-toolchain.toml names) for the three iOS
  targets. A crate's name alone proves nothing: a crates.io hashbrown, memchr or libc that both
  libraries built the same way would share names but not the sysroot's disambiguator. Legacy
  mangled names (`__ZN...`) count only when the exact name is defined in the sysroot. Every other
  Rust overlap is REVIEW. Toolchain symbols must still pass the member or code comparison above.
- The allocator and panic shims rustc generates (`___rust_alloc` and friends): must pass the
  member or code comparison above. The gate also fails when any library defines `__rg_alloc` or
  its siblings (a `#[global_allocator]`): the shims would then forward to that allocator, and the
  linker would bind every library to one library's choice.

A symbol defined strongly in two members whose code is identical is listed as DUPLICATE: the
linker keeps the first member it loads, drops the other and prints `ld: warning: duplicate
symbol` without failing (ld-27037.1 of Xcode 27.1 with -ObjC -dead_strip, tested 2026-10-16), so
the app then runs one library's copy for both. The app's link must fail on any such warning
outside an allowlist of exactly these names.

Exit status 0 only when every overlap is IDENTICAL or IDENTICAL CODE and nothing above failed.
Reads the archives and the sysroot only.
"""

import collections
import os
import re
import struct
import subprocess
import sys

MH_MAGIC_64 = 0xFEEDFACF
FAT_MAGIC = 0xCAFEBABE
FAT_MAGIC_64 = 0xCAFEBABF
LC_SEGMENT_64 = 0x19
LC_SYMTAB = 0x2
LC_BUILD_VERSION = 0x32
LC_VERSION_MIN = {0x24: "macOS", 0x25: "iOS", 0x2F: "tvOS", 0x30: "watchOS"}
PLATFORMS = {1: "macOS", 2: "iOS", 3: "tvOS", 4: "watchOS", 6: "Mac Catalyst",
             7: "iOS simulator", 8: "tvOS simulator", 9: "watchOS simulator"}
CPU_TYPES = {0x0100000C: "arm64", 0x01000007: "x86_64"}
N_STAB = 0xE0
N_PEXT = 0x10
N_TYPE = 0x0E
N_EXT = 0x01
N_SECT = 0x0E
N_WEAK_DEF = 0x0080

SYSROOT_TARGETS = ("aarch64-apple-ios", "aarch64-apple-ios-sim", "x86_64-apple-ios")

# Symbols rustc generates into every static library (allocator and panic shims).
KNOWN_SHIMS = re.compile(
    r"^_+(rust_alloc|rust_dealloc|rust_realloc|rust_alloc_zeroed|rust_alloc_error_handler"
    r"|rust_alloc_error_handler_should_panic(_v2)?|rust_no_alloc_shim_is_unstable(_v2)?"
    r"|rust_start_panic|rust_panic_cleanup|rust_eh_personality|rdl_[a-z_]+|rg_oom)$"
)
# A #[global_allocator]'s functions, which the shims forward to when one exists.
GLOBAL_ALLOCATOR = re.compile(r"^_+rg_(alloc|dealloc|realloc|alloc_zeroed)$")

DEBUG_SEGMENTS = {"__DWARF"}


def ar_members(data, label):
    """(name, bytes) of every member of a BSD ar archive, symbol table excluded."""
    if data[:8] != b"!<arch>\n":
        sys.exit(f"{label}: not an ar archive")
    pos = 8
    members = []
    while pos + 60 <= len(data):
        header = data[pos:pos + 60]
        name = header[:16].decode().strip()
        size = int(header[48:58].decode().strip())
        body = data[pos + 60:pos + 60 + size]
        if name.startswith("#1/"):
            n = int(name[3:])
            name = body[:n].rstrip(b"\0").decode()
            body = body[n:]
        if not name.startswith("__.SYMDEF"):
            members.append((name, body))
        pos += 60 + size + (size & 1)
    return members


def archive_slices(path):
    """{architecture: ar archive bytes} of a plain or fat (universal) archive."""
    with open(path, "rb") as f:
        data = f.read()
    magic = struct.unpack_from(">I", data, 0)[0]
    if magic in (FAT_MAGIC, FAT_MAGIC_64):
        slices = {}
        for i in range(struct.unpack_from(">I", data, 4)[0]):
            if magic == FAT_MAGIC:
                cputype, _sub, offset, size, _align = struct.unpack_from(
                    ">iiIII", data, 8 + 20 * i)
            else:
                cputype, _sub, offset, size, _align, _res = struct.unpack_from(
                    ">iiQQII", data, 8 + 32 * i)
            arch = CPU_TYPES.get(cputype, f"cpu {cputype:#x}")
            slices[arch] = data[offset:offset + size]
        return slices
    for _name, body in ar_members(data, path):
        if len(body) >= 8 and struct.unpack_from("<I", body, 0)[0] == MH_MAGIC_64:
            cputype = struct.unpack_from("<i", body, 4)[0]
            return {CPU_TYPES.get(cputype, f"cpu {cputype:#x}"): data}
    sys.exit(f"{path}: no 64-bit Mach-O member")


class MachO:
    """The parts of a 64-bit Mach-O object file the gate compares."""

    def __init__(self, name, data):
        self.name = name
        self.data = data
        magic, _cpu, _sub, _filetype, ncmds, _size, _flags, _res = struct.unpack_from(
            "<IiiIIIII", data, 0)
        if magic != MH_MAGIC_64:
            raise ValueError(f"{name}: not a 64-bit Mach-O object")
        self.sections = []  # (segname, sectname, addr, size, offset, reloff, nreloc, flags)
        self.symbols = []  # (name, type, sect, desc, value)
        # (platform, minimum OS as "major.minor", load command), or None when the object has
        # neither LC_BUILD_VERSION nor an LC_VERSION_MIN_* command.
        self.min_os = None
        self._comparable = None
        off = 32
        for _ in range(ncmds):
            cmd, cmdsize = struct.unpack_from("<II", data, off)
            if cmd == LC_SEGMENT_64:
                nsects = struct.unpack_from("<I", data, off + 64)[0]
                s = off + 72
                for _ in range(nsects):
                    sectname = data[s:s + 16].rstrip(b"\0").decode()
                    segname = data[s + 16:s + 32].rstrip(b"\0").decode()
                    addr, size, offset, _align, reloff, nreloc, flags = struct.unpack_from(
                        "<QQIIIII", data, s + 32)
                    self.sections.append(
                        (segname, sectname, addr, size, offset, reloff, nreloc, flags))
                    s += 80
            elif cmd == LC_SYMTAB:
                symoff, nsyms, stroff, _strsize = struct.unpack_from("<IIII", data, off + 8)
                for i in range(nsyms):
                    strx, ntype, sect, desc, value = struct.unpack_from(
                        "<IBBHQ", data, symoff + 16 * i)
                    end = data.index(b"\0", stroff + strx)
                    self.symbols.append(
                        (data[stroff + strx:end].decode(errors="replace"), ntype, sect, desc,
                         value))
            elif cmd == LC_BUILD_VERSION:
                platform, minos, _sdk = struct.unpack_from("<III", data, off + 8)
                self.min_os = (PLATFORMS.get(platform, f"platform {platform}"),
                               version(minos), "LC_BUILD_VERSION")
            elif cmd in LC_VERSION_MIN:
                minos, _sdk = struct.unpack_from("<II", data, off + 8)
                self.min_os = (LC_VERSION_MIN[cmd], version(minos), "LC_VERSION_MIN")
            off += cmdsize

    def defined_globals(self):
        for name, ntype, sect, desc, value in self.symbols:
            if ntype & N_STAB or (ntype & N_TYPE) != N_SECT or not ntype & N_EXT:
                continue
            yield name, sect, desc, value, bool(ntype & N_PEXT)

    def defined_names(self):
        for name, ntype, _sect, _desc, _value in self.symbols:
            if not ntype & N_STAB and (ntype & N_TYPE) == N_SECT:
                yield name

    def relocations(self, index):
        """Relocations of section `index` (0-based), targets by name."""
        _seg, _sect, _addr, _size, _offset, reloff, nreloc, _flags = self.sections[index]
        out = []
        for i in range(nreloc):
            address, info = struct.unpack_from("<iI", self.data, reloff + 8 * i)
            symbolnum = info & 0xFFFFFF
            pcrel = (info >> 24) & 1
            length = (info >> 25) & 3
            extern = (info >> 27) & 1
            rtype = (info >> 28) & 0xF
            if rtype == 10:  # ARM64_RELOC_ADDEND: symbolnum is the addend
                target = ("addend", symbolnum)
            elif extern:
                target = ("symbol", self.symbols[symbolnum][0])
            else:
                seg, sect = self.sections[symbolnum - 1][:2] if symbolnum else ("", "")
                target = ("section", seg, sect)
            out.append((address, pcrel, length, rtype, target))
        return out

    def atom(self, symbol, depth=4):
        """The bytes from `symbol` to the next symbol in its section and the relocations in
        that range, offsets made relative to the symbol. A relocation to a global symbol keeps
        its name, with Rust crate disambiguators masked (the same crate gets a different one in
        each library); one to a local symbol (anonymous constants, outlined code) stands for
        that symbol's own atom, compared the same way, to `depth` levels."""
        defs = [s for s in self.symbols
                if s[0] == symbol and not s[1] & N_STAB and (s[1] & N_TYPE) == N_SECT]
        if len(defs) != 1:
            return None
        _name, _ntype, sect, _desc, value = defs[0]
        seg, sectname, addr, size, offset, _r, _n, flags = self.sections[sect - 1]
        starts = sorted({v for n, t, sc, _d, v in self.symbols
                         if sc == sect and not t & N_STAB and (t & N_TYPE) == N_SECT
                         and not n.startswith("L") and v > value})
        end = starts[0] if starts else addr + size
        zerofill = (flags & 0xFF) in (0x1, 0xC, 0x12)
        start_off = value - addr
        content = (b"" if zerofill
                   else self.data[offset + start_off:offset + start_off + (end - value)])
        relocs = []
        for address, pcrel, length, rtype, target in self.relocations(sect - 1):
            if start_off <= address < start_off + (end - value):
                if target[0] == "symbol":
                    target = self._target(target[1], depth)
                relocs.append((address - start_off, pcrel, length, rtype, target))
        return (seg, sectname, end - value, content, tuple(sorted(relocs, key=repr)))

    def _target(self, name, depth):
        is_global = any(s[0] == name and s[1] & N_EXT for s in self.symbols)
        if is_global or depth == 0:
            masked = re.sub(r"Cs[0-9A-Za-z]+_", "Cs_", name)
            return ("symbol", re.sub(r"\.[0-9a-f]{32}\.", ".*.", masked))
        return ("local", self.atom(name, depth - 1))

    def comparable(self):
        """Every non-debug section's bytes and relocations, and the defined symbols."""
        if self._comparable is None:
            sections = []
            for i, (seg, sect, addr, size, offset, _r, _n, flags) in enumerate(self.sections):
                if seg in DEBUG_SEGMENTS:
                    continue
                zerofill = (flags & 0xFF) in (0x1, 0xC, 0x12)
                content = b"" if zerofill else self.data[offset:offset + size]
                sections.append((seg, sect, size, flags, content, tuple(self.relocations(i))))
            symbols = sorted(
                (name, ntype, sect, desc, value)
                for name, ntype, sect, desc, value in self.symbols
                if not ntype & N_STAB)
            self._comparable = (sections, symbols)
        return self._comparable


def version(packed):
    return f"{packed >> 16}.{(packed >> 8) & 0xFF}"


def load(data, label):
    members = []
    for name, body in ar_members(data, label):
        try:
            members.append(MachO(name, body))
        except (ValueError, struct.error):
            pass
    index = collections.defaultdict(list)
    for member in members:
        for name, sect, desc, value, private in member.defined_globals():
            index[name].append((member, sect, desc, value, private))
    return members, index


def crate_roots(symbol):
    """Crate roots in a v0-mangled Rust symbol, as (disambiguator, name): `C` [`s`
    <disambiguator> `_`] <length> [`_`] <name>, the `_` after the length being present when the
    name starts with `_` or a digit. `f16` and `f128` appear as crate roots too: they are
    primitive types without a one-letter code, not crates."""
    roots = set()
    for m in re.finditer(r"C(?:s([0-9A-Za-z]*)_)?(\d+)(_?)", symbol):
        n = int(m.group(2))
        start = m.end()
        name = symbol[start:start + n]
        if m.group(3) and not name.startswith("_") and not name[:1].isdigit():
            # The `_` belonged to the name itself, not a separator.
            start -= 1
            name = symbol[start:start + n]
        if len(name) == n and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
            roots.add((m.group(1) or "", name))
    return {root for root in roots if root[1] not in ("f16", "f128")}


def is_rust(symbol):
    return symbol.startswith("__R") or symbol.startswith("__ZN")


def is_shim(name, roots):
    return (bool(KNOWN_SHIMS.match(name))
            or (is_rust(name) and {root[1] for root in roots} == {"__rustc"}))


class Sysroot:
    """What the toolchain's own prebuilt crates define: the crate roots (disambiguator and
    name) of every symbol defined in its sysroot rlibs for the iOS targets, and the exact names
    (for legacy mangling)."""

    def __init__(self, path):
        self.path = path
        self.roots = set()
        self.names = set()
        self.rlibs = 0
        for target in SYSROOT_TARGETS:
            libdir = os.path.join(path, "lib", "rustlib", target, "lib")
            if not os.path.isdir(libdir):
                sys.exit(f"{libdir}: missing (install the {target} target of this toolchain)")
            for entry in sorted(os.listdir(libdir)):
                if not entry.endswith(".rlib"):
                    continue
                self.rlibs += 1
                with open(os.path.join(libdir, entry), "rb") as f:
                    data = f.read()
                members, _index = load(data, entry)
                for member in members:
                    for name in member.defined_names():
                        if name.startswith("__R"):
                            self.roots |= crate_roots(name)
                        elif name.startswith("__ZN"):
                            self.names.add(name)

    def owns(self, name):
        if name.startswith("__R"):
            roots = crate_roots(name)
            return bool(roots) and roots <= self.roots
        return name in self.names


def default_sysroot():
    here = os.path.dirname(os.path.abspath(__file__))
    run = lambda *args: subprocess.run(  # noqa: E731
        ["rustc", *args], cwd=here, check=True, capture_output=True, text=True).stdout
    release = next((line for line in run("-vV").splitlines() if line.startswith("release:")),
                   "release: ?")
    return run("--print", "sysroot").strip(), release


def dominant_platform(members):
    platforms = collections.Counter(
        m.min_os[0] for m in members if m.min_os and m.min_os[2] == "LC_BUILD_VERSION")
    return platforms.most_common(1)[0][0] if platforms else None


def min_os_summary(members):
    counts = collections.Counter(m.min_os[:2] + (m.min_os[2],) if m.min_os else None
                                 for m in members)
    parts = []
    for key, count in sorted(counts.items(), key=lambda kv: (kv[0] is None, str(kv[0]))):
        if key is None:
            parts.append(f"no version load command x{count}")
        else:
            platform, minos, command = key
            parts.append(f"{platform} {minos} ({command}) x{count}")
    return ", ".join(parts)


def compare_slice(new_members, new_index, old_members, old_index, sysroot):
    """Prints the verdicts for one architecture; returns the number of failures."""
    failures = 0
    overlap = sorted(set(new_index) & set(old_index))
    new_platform = dominant_platform(new_members)
    old_platform = dominant_platform(old_members)
    print(f"    platform: new {new_platform}, pinned {old_platform}; "
          f"{len(old_index)} defined global symbols, {len(overlap)} also defined by the new "
          f"library")
    if new_platform and old_platform and new_platform != old_platform:
        print(f"    FAIL: the new slice is built for {new_platform}, the pinned one for "
              f"{old_platform}; compare device with device and simulator with simulator")
        failures += 1
    for side, index in (("new", new_index), ("pinned", old_index)):
        allocators = sorted(name for name in index if GLOBAL_ALLOCATOR.match(name))
        if allocators:
            print(f"    FAIL: the {side} library defines a global allocator "
                  f"({', '.join(allocators)}); every library's allocator shims would bind to it")
            failures += 1

    verdicts = collections.Counter()
    review = []
    duplicates = []
    compared = {"new": {}, "pinned": {}}
    for name in overlap:
        new_defs = new_index[name]
        old_defs = old_index[name]
        weak = all(d[2] & N_WEAK_DEF for d in new_defs + old_defs)
        roots = crate_roots(name) if name.startswith("__R") else set()
        if is_shim(name, roots):
            family = "rustc shim"
        elif is_rust(name):
            if not sysroot.owns(name):
                verdicts["REVIEW (Rust, not the toolchain's own crates)"] += 1
                review.append((name, "Rust symbol whose crate roots "
                               f"{sorted(roots) or '(legacy name)'} are not all the sysroot's"))
                continue
            family = "Rust toolchain crates"
        elif name.startswith("_ring_core_"):
            family = "ring"
        elif "secp256k1" in name:
            family = "secp256k1"
        else:
            family = "compiler-rt / C runtime"

        for new_def in new_defs:
            compared["new"][id(new_def[0])] = new_def[0]
        for old_def in old_defs:
            compared["pinned"][id(old_def[0])] = old_def[0]
        if any(new_def[0].comparable() == old_def[0].comparable()
               for new_def in new_defs for old_def in old_defs):
            verdicts[f"IDENTICAL ({family})"] += 1
            continue
        # The members differ: compare this symbol's own code.
        same_code = any(
            new_def[0].atom(name) is not None
            and new_def[0].atom(name) == old_def[0].atom(name)
            for new_def in new_defs for old_def in old_defs)
        if same_code:
            verdicts[f"IDENTICAL CODE ({family}, in differing members)"] += 1
            if not weak:
                duplicates.append((name, new_defs[0][0].name, old_defs[0][0].name))
        else:
            verdicts[f"REVIEW ({family}, code differs)"] += 1
            review.append((name, f"{family}: neither the defining members "
                           f"({new_defs[0][0].name} vs {old_defs[0][0].name}) nor the "
                           f"symbol's own code compare equal; "
                           f"{'weak' if weak else 'strong'} definitions"))

    for verdict, count in sorted(verdicts.items()):
        print(f"    {count:6d}  {verdict}")
    for side in ("new", "pinned"):
        members = list(compared[side].values())
        if members:
            print(f"    minimum OS of the {len(members)} compared {side} members: "
                  f"{min_os_summary(members)}")
    for name, new_member, old_member in duplicates:
        print(f"    DUPLICATE {name}: the same code, defined strongly in {new_member} and "
              f"{old_member}; ld keeps the first member it loads and warns 'duplicate symbol' "
              f"(allowlist it in the app's link check)")
    for name, why in review[:40]:
        print(f"    REVIEW {name}: {why}")
    if len(review) > 40:
        print(f"    ... and {len(review) - 40} more")
    return failures + len(review)


def main(argv):
    sysroot_path = None
    if len(argv) >= 2 and argv[0] == "--sysroot":
        sysroot_path, argv = argv[1], argv[2:]
    if len(argv) < 2:
        sys.exit(__doc__)
    new_path, pinned_paths = argv[0], argv[1:]

    if sysroot_path is None:
        sysroot_path, release = default_sysroot()
        print(f"sysroot: {sysroot_path} (rustc {release})")
    else:
        print(f"sysroot: {sysroot_path}")
    sysroot = Sysroot(sysroot_path)
    print(f"  {sysroot.rlibs} rlibs for {', '.join(SYSROOT_TARGETS)}: "
          f"{len(sysroot.roots)} crate roots, {len(sysroot.names)} legacy names")

    new_slices = {arch: load(data, f"{new_path} ({arch})")
                  for arch, data in archive_slices(new_path).items()}
    print(f"new: {new_path}: "
          + ", ".join(f"{arch} ({len(index)} defined global symbols, "
                      f"{dominant_platform(members)})"
                      for arch, (members, index) in sorted(new_slices.items())))
    failures = 0
    for pinned_path in pinned_paths:
        pinned_slices = archive_slices(pinned_path)
        print(f"\npinned: {pinned_path}: {', '.join(sorted(pinned_slices))}")
        common = sorted(set(new_slices) & set(pinned_slices))
        for arch in sorted(set(new_slices) ^ set(pinned_slices)):
            side = "new" if arch in new_slices else "pinned"
            print(f"  {arch}: only in the {side} library, not compared")
        if not common:
            print("  FAIL: no architecture in common with the new library")
            failures += 1
        for arch in common:
            print(f"  {arch}:")
            old_members, old_index = load(pinned_slices[arch], f"{pinned_path} ({arch})")
            new_members, new_index = new_slices[arch]
            failures += compare_slice(new_members, new_index, old_members, old_index, sysroot)
    print("\nRESULT: " + ("PASS: every overlap is identical (members or code)"
                          if failures == 0 else "FAIL: overlaps need review"))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
