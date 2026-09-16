#!/usr/bin/env python3
"""Release-profile parity, provenance JSON, and builder-control regressions."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import tomllib
import unittest


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


profile_json = load("release_profile_json", HERE / "release-profile-json.py")
archive = load("release_profile_archive", HERE / "release-profile-archive.py")
measure = load("measure_release_profile", HERE / "measure-release-profile.py")

CANONICAL = {
    "opt_level": "3",
    "debug": "0",
    "debug_assertions": False,
    "overflow_checks": False,
    "lto": "thin",
    "codegen_units": 1,
    "panic": "unwind",
    "strip": "none",
}
BASELINE_CONTROL = {"lto": False, "codegen_units": 16}


def env_file_values(path: Path) -> dict[str, str]:
    values = {}
    for line in path.read_text().splitlines():
        if not line.startswith("export "):
            continue
        name, _, value = line[len("export ") :].partition("=")
        values[name] = value
    return values


def write_ar(path: Path, members: list[tuple[str, bytes]]) -> None:
    blob = bytearray(b"!<arch>\n")
    for name, content in members:
        encoded = name.encode("ascii")
        if len(encoded) > 16:
            raise ValueError(name)
        header = (
            encoded.ljust(16)
            + b"0".ljust(12)
            + b"0".ljust(6)
            + b"0".ljust(6)
            + b"644".ljust(8)
            + f"{len(content)}".encode().rjust(10)
            + b"`\n"
        )
        blob.extend(header)
        blob.extend(content)
        if len(content) % 2 == 1:
            blob.append(0)
    path.write_bytes(blob)


def write_executable(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(path.stat().st_mode | stat.S_IEXEC)


class ReleaseProfileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_root_and_canonical_thin_parity(self):
        cargo = tomllib.loads((ROOT / "Cargo.toml").read_text())
        release = cargo["profile"]["release"]
        self.assertEqual(release["lto"], "thin")
        self.assertEqual(release["codegen-units"], 1)
        self.assertEqual(release["strip"], "none")
        self.assertNotIn("panic", release)
        env = env_file_values(HERE / "marmotkit-release-profile.env")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_LTO"], "thin")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"], "1")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_STRIP"], "none")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_DEBUG"], "0")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_PANIC"], "unwind")
        self.assertEqual(profile_json.profile_from_env(env), CANONICAL)
        self.assertNotEqual(CANONICAL["lto"], BASELINE_CONTROL["lto"])
        self.assertNotEqual(CANONICAL["codegen_units"], BASELINE_CONTROL["codegen_units"])

    def test_standard_release_path_and_symbol_policy(self):
        cargo = (ROOT / "Cargo.toml").read_text()
        self.assertNotIn("[profile.release-size]", cargo)
        self.assertNotIn("lto = \"fat\"", cargo)
        env_text = (HERE / "marmotkit-release-profile.env").read_text()
        self.assertIn("strip=none", env_text)
        self.assertIn("Android target builds", env_text)
        for script in ("kotlin-bindings.sh", "xcframework.sh", "xcframework-macos.sh"):
            text = (HERE / script).read_text()
            self.assertIn('source "$TOOL_DIR/marmotkit-release-profile.env"', text)
        kotlin = (HERE / "kotlin-bindings.sh").read_text()
        self.assertIn("CARGO_PROFILE_RELEASE_STRIP=symbols", kotlin)
        self.assertIn("host dylib must keep its symbol table", kotlin)
        apple = (HERE / "xcframework.sh").read_text() + (HERE / "xcframework-macos.sh").read_text()
        self.assertNotIn("CARGO_PROFILE_RELEASE_STRIP=symbols", apple)

    def test_serializers_type_false_and_thin_lto(self):
        env = env_file_values(HERE / "marmotkit-release-profile.env")
        thin = profile_json.profile_from_env(env)
        self.assertEqual(thin["lto"], "thin")
        self.assertIsInstance(thin["codegen_units"], int)
        control = dict(env)
        control["CARGO_PROFILE_RELEASE_LTO"] = "false"
        control["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"] = "16"
        parsed = profile_json.profile_from_env(control)
        self.assertIs(parsed["lto"], False)
        self.assertEqual(parsed["codegen_units"], 16)
        self.assertEqual(parsed["strip"], "none")
        self.assertEqual(parsed["panic"], "unwind")
        encoded = json.loads(profile_json.profile_json(control))
        self.assertIs(encoded["lto"], False)

    def test_serializers_reject_unknown_and_malformed_values(self):
        env = env_file_values(HERE / "marmotkit-release-profile.env")
        for name, value in [
            ("CARGO_PROFILE_RELEASE_LTO", "thin\n, \"oops\""),
            ("CARGO_PROFILE_RELEASE_LTO", "full"),
            ("CARGO_PROFILE_RELEASE_CODEGEN_UNITS", "1; rm -rf /"),
            ("CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS", "FALSE"),
            ("CARGO_PROFILE_RELEASE_STRIP", ""),
        ]:
            bad = dict(env)
            bad[name] = value
            with self.subTest(name=name, value=value), self.assertRaises(profile_json.ProfileError):
                profile_json.profile_from_env(bad)
        missing = dict(env)
        del missing["CARGO_PROFILE_RELEASE_LTO"]
        with self.assertRaises(profile_json.ProfileError):
            profile_json.profile_from_env(missing)

    def test_package_scripts_use_shared_serializer(self):
        for name in ("package-ios-artifacts.sh", "package-macos-artifacts.sh"):
            text = (HERE / name).read_text()
            self.assertIn("release-profile-json.py", text)
            self.assertNotIn("lto\": $CARGO_PROFILE_RELEASE_LTO", text)

    def test_archive_rejects_bitcode_and_keeps_duplicate_names(self):
        native = self.root / "native.a"
        bitcode = self.root / "bitcode.a"
        mixed = self.root / "mixed.a"
        write_ar(native, [("obj.o", b"\xcf\xfa\xed\xfe" + b"native-object")])
        write_ar(bitcode, [("obj.o", archive.LLVM_BITCODE_MAGIC + b"ir")])
        write_ar(
            mixed,
            [
                ("dup.o", b"\xcf\xfa\xed\xfe" + b"first"),
                ("dup.o", archive.LLVM_WRAPPER_MAGIC + b"second"),
            ],
        )
        self.assertGreater(archive.check_archive(native), 0)
        with self.assertRaises(archive.ArchiveError):
            archive.check_archive(bitcode)
        with self.assertRaisesRegex(archive.ArchiveError, "member 1"):
            archive.check_archive(mixed)
        otool = "Archive : mixed.a(dup.o)\n  sectname __bitcode\n  segname __LLVM\n"
        with self.assertRaises(archive.ArchiveError):
            archive.check_archive(native, otool)

    def test_builders_keep_android_strip_isolated(self):
        log = self.root / "cargo.jsonl"
        log.write_text("")
        bin_dir = self.root / "bin"
        target = self.root / "target"
        workspace = self.root / "old-source"
        crate = workspace / "crates/marmot-uniffi"
        (crate / "kotlin-support/dev/ipf/marmotkit").mkdir(parents=True)
        (crate / "kotlin-support/dev/ipf/marmotkit/MarmotAndroid.kt").write_text("class MarmotAndroid\n")
        (crate / "kotlin-support/io/crates/keyring").mkdir(parents=True)
        (crate / "kotlin-support/io/crates/keyring/Keyring.kt").write_text("class Keyring\n")
        (crate / "marmotkit-release-profile.env").write_text(
            "export CARGO_PROFILE_RELEASE_LTO=false\n"
            "export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16\n"
            "export CARGO_PROFILE_RELEASE_STRIP=none\n"
        )
        write_executable(
            bin_dir / "rustup",
            "#!/bin/sh\n"
            "if [ \"$1\" = target ] && [ \"$2\" = list ]; then\n"
            "  printf '%s\\n' aarch64-linux-android armv7-linux-androideabi "
            "i686-linux-android x86_64-linux-android\n"
            "fi\n",
        )
        write_executable(
            bin_dir / "cargo",
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            f"log = Path({str(log)!r})\n"
            "entry = {'argv': sys.argv[1:], 'strip': os.environ.get('CARGO_PROFILE_RELEASE_STRIP'),"
            " 'lto': os.environ.get('CARGO_PROFILE_RELEASE_LTO'),"
            " 'codegen': os.environ.get('CARGO_PROFILE_RELEASE_CODEGEN_UNITS')}\n"
            "with log.open('a', encoding='utf-8') as handle:\n"
            "    handle.write(json.dumps(entry) + '\\n')\n"
            "args = sys.argv[1:]\n"
            "target_dir = Path(os.environ['CARGO_TARGET_DIR'])\n"
            "if args and args[0] == 'build':\n"
            "    triple = None\n"
            "    if '--target' in args:\n"
            "        triple = args[args.index('--target') + 1]\n"
            "    if triple:\n"
            "        out = target_dir / triple / 'release' / 'libmarmot_uniffi.so'\n"
            "    else:\n"
            "        out = target_dir / 'release' / 'libmarmot_uniffi.so'\n"
            "    out.parent.mkdir(parents=True, exist_ok=True)\n"
            "    out.write_bytes(b'library')\n"
            "elif args and args[0] == 'run':\n"
            "    out_dir = Path(args[args.index('--out-dir') + 1])\n"
            "    generated = out_dir / 'dev/ipf/marmotkit/marmot_uniffi.kt'\n"
            "    generated.parent.mkdir(parents=True, exist_ok=True)\n"
            "    generated.write_text('generated')\n"
            "else:\n"
            "    raise SystemExit('unexpected cargo invocation')\n",
        )
        ndk = self.root / "ndk/toolchains/llvm/prebuilt/linux-x86_64/bin"
        for name in (
            "aarch64-linux-android26-clang",
            "armv7a-linux-androideabi26-clang",
            "i686-linux-android26-clang",
            "x86_64-linux-android26-clang",
            "llvm-ar",
            "llvm-ranlib",
        ):
            write_executable(ndk / name, "#!/bin/sh\nexit 0\n")
        write_executable(
            ndk / "llvm-readelf",
            "#!/bin/sh\necho 'There are 4 section headers'\n",
        )
        env = os.environ.copy()
        env["HOME"] = str(self.root / "home-kotlin")
        env["PATH"] = f"{bin_dir}:{env['PATH']}"
        env["ANDROID_NDK_HOME"] = str(self.root / "ndk")
        env["CARGO_TARGET_DIR"] = str(target)
        env["MARMOTKIT_WORKSPACE_DIR"] = str(workspace)
        env["MARMOTKIT_CRATE_DIR"] = str(crate)
        env["OTLP_EXPORT"] = "1"
        env["PRODUCT_ANALYTICS_EXPORT"] = "1"
        subprocess.run(
            [str(HERE / "kotlin-bindings.sh")],
            check=True,
            env=env,
            cwd=ROOT,
        )
        records = [json.loads(line) for line in log.read_text().splitlines() if line]
        host = [row for row in records if row["argv"][:1] == ["build"] and "--target" not in row["argv"]]
        android = [row for row in records if "--target" in row["argv"]]
        generate = [row for row in records if row["argv"][:1] == ["run"]]
        self.assertEqual(len(host), 1)
        self.assertEqual(host[0]["strip"], "none")
        self.assertEqual(host[0]["lto"], "thin")
        self.assertEqual(host[0]["codegen"], "1")
        self.assertEqual(len(android), 4)
        for row in android:
            self.assertEqual(row["strip"], "symbols")
            self.assertEqual(row["lto"], "thin")
            self.assertEqual(row["codegen"], "1")
        self.assertEqual(len(generate), 1)
        self.assertTrue((crate / "output/android/kotlin/dev/ipf/marmotkit/marmot_uniffi.kt").is_file())

    def test_kotlin_generation_is_required(self):
        log = self.root / "cargo.jsonl"
        log.write_text("")
        bin_dir = self.root / "bin"
        workspace = self.root / "old-source"
        crate = workspace / "crates/marmot-uniffi"
        (crate / "kotlin-support/dev/ipf/marmotkit").mkdir(parents=True)
        (crate / "kotlin-support/io/crates/keyring").mkdir(parents=True)
        write_executable(
            bin_dir / "rustup",
            "#!/bin/sh\nprintf '%s\\n' aarch64-linux-android armv7-linux-androideabi "
            "i686-linux-android x86_64-linux-android\n",
        )
        write_executable(
            bin_dir / "cargo",
            "#!/usr/bin/env python3\n"
            "import os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "target_dir = Path(os.environ['CARGO_TARGET_DIR'])\n"
            "if args and args[0] == 'build' and '--target' not in args:\n"
            "    out = target_dir / 'release' / 'libmarmot_uniffi.so'\n"
            "    out.parent.mkdir(parents=True, exist_ok=True)\n"
            "    out.write_bytes(b'library')\n"
            "elif args and args[0] == 'run':\n"
            "    raise SystemExit(0)\n"
            "else:\n"
            "    raise SystemExit('unexpected cargo invocation')\n",
        )
        ndk = self.root / "ndk/toolchains/llvm/prebuilt/linux-x86_64/bin"
        ndk.mkdir(parents=True)
        env = os.environ.copy()
        env["HOME"] = str(self.root / "home-kotlin-required")
        env["PATH"] = f"{bin_dir}:{env['PATH']}"
        env["ANDROID_NDK_HOME"] = str(self.root / "ndk")
        env["CARGO_TARGET_DIR"] = str(self.root / "target")
        env["MARMOTKIT_WORKSPACE_DIR"] = str(workspace)
        env["MARMOTKIT_CRATE_DIR"] = str(crate)
        completed = subprocess.run(
            [str(HERE / "kotlin-bindings.sh")],
            env=env,
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("produced no Kotlin binding", completed.stderr)

    def test_apple_builders_keep_symbols_and_builder_owned_profile(self):
        for script, extras in (
            (
                "xcframework.sh",
                {
                    "targets": ["aarch64-apple-ios", "aarch64-apple-ios-sim"],
                    "output": "output/MarmotKit.xcframework",
                },
            ),
            (
                "xcframework-macos.sh",
                {
                    "targets": ["aarch64-apple-darwin"],
                    "output": "output/macos/MarmotKit.xcframework",
                },
            ),
        ):
            with self.subTest(script=script):
                self._assert_apple_builder_profile(script, extras)

    def _assert_apple_builder_profile(self, script: str, extras: dict):
        log = self.root / f"{script}.jsonl"
        log.write_text("")
        bin_dir = self.root / f"bin-{script}"
        workspace = self.root / f"old-source-{script}"
        crate = workspace / "crates/marmot-uniffi"
        privacy = crate / "apple-privacy"
        shutil.copytree(HERE / "apple-privacy", privacy)
        (crate / "marmotkit-release-profile.env").write_text(
            "export CARGO_PROFILE_RELEASE_LTO=false\n"
            "export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16\n"
            "export CARGO_PROFILE_RELEASE_STRIP=none\n"
        )
        write_executable(
            bin_dir / "rustup",
            "#!/bin/sh\nexit 0\n",
        )
        write_executable(
            bin_dir / "xcodebuild",
            "#!/bin/sh\n"
            "out=''\n"
            "while [ $# -gt 0 ]; do\n"
            "  if [ \"$1\" = -output ]; then out=$2; fi\n"
            "  shift\n"
            "done\n"
            "mkdir -p \"$out\"\n"
            "printf 'xcframework' > \"$out/Info.plist\"\n",
        )
        write_executable(
            bin_dir / "cargo",
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            f"log = Path({str(log)!r})\n"
            "entry = {'argv': sys.argv[1:], 'strip': os.environ.get('CARGO_PROFILE_RELEASE_STRIP'),"
            " 'lto': os.environ.get('CARGO_PROFILE_RELEASE_LTO'),"
            " 'codegen': os.environ.get('CARGO_PROFILE_RELEASE_CODEGEN_UNITS')}\n"
            "with log.open('a', encoding='utf-8') as handle:\n"
            "    handle.write(json.dumps(entry) + '\\n')\n"
            "args = sys.argv[1:]\n"
            f"workspace = Path({str(workspace)!r})\n"
            "target_dir = workspace / 'target'\n"
            "if args and args[0] == 'build':\n"
            "    triple = args[args.index('--target') + 1] if '--target' in args else None\n"
            "    if triple:\n"
            "        out = target_dir / triple / 'release' / 'libmarmot_uniffi.a'\n"
            "    else:\n"
            "        out = target_dir / 'release' / 'libmarmot_uniffi.dylib'\n"
            "    out.parent.mkdir(parents=True, exist_ok=True)\n"
            "    out.write_bytes(b'archive' if triple else b'host')\n"
            "elif args and args[0] == 'run':\n"
            "    out_dir = Path(args[args.index('--out-dir') + 1])\n"
            "    out_dir.mkdir(parents=True, exist_ok=True)\n"
            "    (out_dir / 'marmot_uniffi.swift').write_text('swift')\n"
            "    (out_dir / 'marmot_uniffiFFI.h').write_text('header')\n"
            "    (out_dir / 'marmot_uniffiFFI.modulemap').write_text('module')\n"
            "else:\n"
            "    raise SystemExit('unexpected cargo invocation')\n",
        )
        env = os.environ.copy()
        env["HOME"] = str(self.root / f"home-{script}")
        env["PATH"] = f"{bin_dir}:{env['PATH']}"
        env["MARMOTKIT_WORKSPACE_DIR"] = str(workspace)
        env["MARMOTKIT_CRATE_DIR"] = str(crate)
        env["OTLP_EXPORT"] = "1"
        env["PRODUCT_ANALYTICS_EXPORT"] = "1"
        subprocess.run([str(HERE / script)], check=True, env=env, cwd=ROOT)
        records = [json.loads(line) for line in log.read_text().splitlines() if line]
        builds = [row for row in records if row["argv"][:1] == ["build"]]
        generate = [row for row in records if row["argv"][:1] == ["run"]]
        self.assertGreaterEqual(len(builds), 1 + len(extras["targets"]))
        for row in records:
            self.assertEqual(row["strip"], "none")
            self.assertEqual(row["lto"], "thin")
            self.assertEqual(row["codegen"], "1")
            self.assertNotIn("CARGO_PROFILE_RELEASE_STRIP=symbols", " ".join(row["argv"]))
        self.assertEqual(len(generate), 1)
        self.assertTrue((crate / extras["output"]).exists())

    def test_profile_workflow_is_non_publishing(self):
        text = (ROOT / ".github/workflows/bindings-profile.yml").read_text()
        self.assertIn("persist-credentials: false", text)
        self.assertIn("contents: read", text)
        self.assertNotIn("pull_request_target", text)
        self.assertNotIn("secrets.", text)
        self.assertNotIn("softprops/action-gh-release", text)
        self.assertNotIn("upload-to-github-release", text)
        self.assertIn("github.event.pull_request.head.sha", text)

    def test_unavailable_measurements_are_not_zero(self):
        row = measure.artifact_row(
            "aarch64-linux-android",
            "android_jni_so",
            "symbols",
            None,
            None,
            "Android NDK or Rust target unavailable",
        )
        self.assertIsNone(row["baseline_bytes"])
        self.assertIsNone(row["candidate_bytes"])
        self.assertIsNone(row["delta_bytes"])
        self.assertEqual(row["availability"], "unavailable")
        self.assertNotEqual(row["baseline_bytes"], 0)
        self.assertNotEqual(row["candidate_bytes"], 0)

    def test_android_reduction_gate_requires_measured_shrink(self):
        missing = [
            {
                "target": "aarch64-linux-android",
                "availability": "unavailable",
                "reason": "Android NDK or Rust target unavailable",
            }
        ]
        self.assertTrue(measure.android_reduction_failures(missing))
        grown = [
            {
                "target": "aarch64-linux-android",
                "availability": "measured",
                "baseline_bytes": 10,
                "candidate_bytes": 12,
                "delta_bytes": 2,
            },
            {
                "target": "armv7-linux-androideabi",
                "availability": "measured",
                "baseline_bytes": 8,
                "candidate_bytes": 7,
                "delta_bytes": -1,
            },
        ]
        failures = measure.android_reduction_failures(grown)
        self.assertEqual(len(failures), 1)
        self.assertIn("aarch64-linux-android", failures[0])
        shrunk = [
            {
                "target": "aarch64-linux-android",
                "availability": "measured",
                "baseline_bytes": 10,
                "candidate_bytes": 8,
                "delta_bytes": -2,
            },
            {
                "target": "armv7-linux-androideabi",
                "availability": "measured",
                "baseline_bytes": 8,
                "candidate_bytes": 7,
                "delta_bytes": -1,
            },
            {
                "target": "x86_64-linux-android",
                "availability": "measured",
                "baseline_bytes": 9,
                "candidate_bytes": 11,
                "delta_bytes": 2,
            },
        ]
        self.assertEqual(measure.android_reduction_failures(shrunk), [])

    def test_measurement_overrides_only_lto_and_codegen(self):
        base = {"PATH": "/bin"}
        env = measure.profile_env(base, "baseline", "symbols")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_LTO"], "false")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"], "16")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_STRIP"], "symbols")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_OPT_LEVEL"], "3")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_DEBUG"], "0")
        self.assertEqual(env["CARGO_PROFILE_RELEASE_PANIC"], "unwind")
        candidate = measure.profile_env(base, "candidate", "none")
        self.assertEqual(candidate["CARGO_PROFILE_RELEASE_LTO"], "thin")
        self.assertEqual(candidate["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"], "1")
        self.assertEqual(candidate["CARGO_PROFILE_RELEASE_STRIP"], "none")

    def test_committed_measurements_are_schema_one(self):
        report = json.loads((HERE / "release-profile-measurements.json").read_text())
        self.assertEqual(report["schema_version"], 1)
        self.assertRegex(report["source_sha"], r"^[0-9a-f]{40}$")
        self.assertRegex(report["builder_sha"], r"^[0-9a-f]{40}$")
        self.assertRegex(report["lock_sha256"], r"^[0-9a-f]{64}$")
        host = next(
            row
            for row in report["artifacts"]
            if row["kind"] == "host_generation_library"
        )
        self.assertEqual(host["availability"], "measured")
        self.assertLess(host["candidate_bytes"], host["baseline_bytes"])
        self.assertIsInstance(host["delta_bytes"], int)
        self.assertNotEqual(host["baseline_bytes"], 0)
        for row in report["artifacts"]:
            if row["availability"] == "unavailable":
                self.assertIsNone(row["baseline_bytes"])
                self.assertIsNone(row["candidate_bytes"])
                self.assertIsNone(row["delta_bytes"])
                self.assertTrue(row["reason"])
        self.assertGreaterEqual(len(report["cpu_runs"]), 2)
        estimates = [
            row["point_estimate"]
            for row in report["cpu_runs"]
            if row.get("point_estimate") is not None
        ]
        self.assertTrue(estimates)


if __name__ == "__main__":
    unittest.main()
