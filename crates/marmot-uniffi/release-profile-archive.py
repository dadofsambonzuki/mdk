#!/usr/bin/env python3
"""Reject LLVM bitcode in packaged Apple static archives.

Walks every archive member, including duplicate names. Extraction that
collapses names is not used. Byte-level bitcode magic is always checked.
When Apple otool output is available, embedded __LLVM / __bitcode sections
are rejected as well.
"""

from __future__ import annotations

import argparse
import os
import shutil
import struct
import subprocess
import sys
from pathlib import Path


AR_MAGIC = b"!<arch>\n"
LLVM_BITCODE_MAGIC = b"BC\xc0\xde"
LLVM_WRAPPER_MAGIC = bytes((0xDE, 0xC0, 0x17, 0x0B))
MACHO_MAGICS = {
    0xFEEDFACE,
    0xCEFAEDFE,
    0xFEEDFACF,
    0xCFFAEDFE,
    0xCAFEBABE,
    0xBEBAFECA,
}
LC_SEGMENT = 0x01
LC_SEGMENT_64 = 0x19


class ArchiveError(ValueError):
    """The archive is malformed or contains unexpected bitcode."""


def iter_ar_members(data: bytes):
    if not data.startswith(AR_MAGIC):
        raise ArchiveError("not a Unix static archive")
    offset = len(AR_MAGIC)
    index = 0
    while offset < len(data):
        if offset + 60 > len(data):
            raise ArchiveError("truncated archive member header")
        header = data[offset : offset + 60]
        if header[58:60] != b"`\n":
            raise ArchiveError("invalid archive member header")
        name = header[0:16].decode("ascii", "replace").rstrip()
        size_field = header[48:58].decode("ascii", "replace").strip()
        if not size_field.isdigit():
            raise ArchiveError(f"invalid archive member size for {name}")
        size = int(size_field)
        offset += 60
        if name.startswith("#1/"):
            name_len = int(name[3:])
            if offset + size > len(data) or name_len > size:
                raise ArchiveError(f"truncated BSD long name for {name}")
            raw_name = data[offset : offset + name_len]
            name = raw_name.split(b"\0", 1)[0].decode("ascii", "replace")
            content = data[offset + name_len : offset + size]
            consumed = size
        else:
            if offset + size > len(data):
                raise ArchiveError(f"truncated archive member {name}")
            content = data[offset : offset + size]
            consumed = size
        yield index, name, content
        index += 1
        offset += consumed
        if offset % 2 == 1:
            offset += 1


def _u32(data: bytes, offset: int, little: bool) -> int:
    fmt = "<I" if little else ">I"
    return struct.unpack_from(fmt, data, offset)[0]


def macho_has_llvm_bitcode(content: bytes) -> bool:
    if len(content) < 8:
        return False
    magic = struct.unpack_from("<I", content, 0)[0]
    if magic not in MACHO_MAGICS and struct.unpack_from(">I", content, 0)[0] not in MACHO_MAGICS:
        return False
    little = magic in {0xCEFAEDFE, 0xCFFAEDFE, 0xBEBAFECA}
    magic = _u32(content, 0, little)
    if magic in {0xCAFEBABE, 0xBEBAFECA}:
        return b"__LLVM" in content or b"__bitcode" in content
    is_64 = magic in {0xFEEDFACF, 0xCFFAEDFE}
    header_size = 32 if is_64 else 28
    if len(content) < header_size:
        return False
    ncmds = _u32(content, 16, little)
    offset = header_size
    for _ in range(ncmds):
        if offset + 8 > len(content):
            return False
        cmd = _u32(content, offset, little)
        cmdsize = _u32(content, offset + 4, little)
        if cmdsize < 8 or offset + cmdsize > len(content):
            return False
        if cmd in {LC_SEGMENT, LC_SEGMENT_64}:
            name_off = offset + 8
            segname = content[name_off : name_off + 16].split(b"\0", 1)[0]
            if segname == b"__LLVM":
                return True
            section_size = 80 if cmd == LC_SEGMENT_64 else 68
            nsects_off = offset + (64 if cmd == LC_SEGMENT_64 else 48)
            if nsects_off + 4 <= offset + cmdsize:
                nsects = _u32(content, nsects_off, little)
                sect = offset + (72 if cmd == LC_SEGMENT_64 else 56)
                for _ in range(nsects):
                    if sect + 32 > offset + cmdsize:
                        break
                    sectname = content[sect : sect + 16].split(b"\0", 1)[0]
                    if sectname == b"__bitcode":
                        return True
                    sect += section_size
        offset += cmdsize
    return False


def member_has_bitcode(content: bytes) -> bool:
    if content.startswith(LLVM_BITCODE_MAGIC) or content.startswith(LLVM_WRAPPER_MAGIC):
        return True
    return macho_has_llvm_bitcode(content)


def otool_output_has_bitcode(text: str) -> bool:
    for line in text.splitlines():
        stripped = line.strip()
        if stripped == "segname __LLVM" or stripped.startswith("segname __LLVM"):
            return True
        if stripped == "sectname __bitcode" or stripped.startswith("sectname __bitcode"):
            return True
    return False


def inspect_otool(path: Path, otool_output: str | None = None) -> None:
    text = otool_output
    if text is None:
        otool = shutil.which("otool")
        xcrun = shutil.which("xcrun")
        if xcrun:
            command = ["xcrun", "otool", "-l", str(path)]
        elif otool:
            command = [otool, "-l", str(path)]
        else:
            return
        text = subprocess.check_output(command, text=True)
    if otool_output_has_bitcode(text):
        raise ArchiveError(f"{path} contains LLVM bitcode in otool load commands")


def check_archive(path: Path, otool_output: str | None = None) -> int:
    data = path.read_bytes()
    members = list(iter_ar_members(data))
    if not members:
        raise ArchiveError(f"{path} contains no archive members")
    for index, name, content in members:
        if member_has_bitcode(content):
            raise ArchiveError(
                f"{path} member {index} ({name}) contains LLVM bitcode"
            )
    inspect_otool(path, otool_output)
    return len(members)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("--otool-output", type=Path)
    args = parser.parse_args(sys.argv[1:] if argv is None else argv)
    os.umask(0o077)
    extra = args.otool_output.read_text() if args.otool_output else None
    try:
        count = check_archive(args.archive, extra)
    except ArchiveError as error:
        sys.stderr.write(f"error: {error}\n")
        return 1
    print(f"Validated {count} native archive member(s) in {args.archive}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
