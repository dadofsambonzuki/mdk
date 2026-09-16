#!/usr/bin/env python3
"""Compare baseline and candidate MarmotKit release-profile artifacts.

Schema version 1. Missing platform data is recorded as unavailable; never as
zero. This script does not publish artifacts or mutate source.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path


CRATE = Path(__file__).resolve().parent
ROOT = CRATE.parents[1]
ANDROID_ABIS = {
    "arm64-v8a": "aarch64-linux-android",
    "armeabi-v7a": "armv7-linux-androideabi",
    "x86": "i686-linux-android",
    "x86_64": "x86_64-linux-android",
}
ANDROID_CLANG_PREFIX = {
    "aarch64-linux-android": "aarch64-linux-android",
    "armv7-linux-androideabi": "armv7a-linux-androideabi",
    "i686-linux-android": "i686-linux-android",
    "x86_64-linux-android": "x86_64-linux-android",
}
PRIMARY_ANDROID_TARGETS = (
    "aarch64-linux-android",
    "armv7-linux-androideabi",
)
APPLE_SLICES = {
    "aarch64-apple-ios": "apple_static_archive",
    "aarch64-apple-ios-sim": "apple_static_archive",
    "aarch64-apple-darwin": "apple_static_archive",
}
FEATURES = ["otlp-export", "product-analytics-export"]
BASELINE_PROFILE = {"lto": False, "codegen_units": 16}
CANDIDATE_PROFILE = {"lto": "thin", "codegen_units": 1}
ANDROID_API = os.environ.get("ANDROID_API", "26")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run(command, env, cwd, log_dir, name):
    started = time.monotonic()
    completed = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    duration = time.monotonic() - started
    (log_dir / f"{name}.stdout").write_text(completed.stdout)
    (log_dir / f"{name}.stderr").write_text(completed.stderr)
    if completed.returncode != 0:
        raise RuntimeError(
            f"{name} failed with {completed.returncode}; see {log_dir / (name + '.stderr')}"
        )
    return duration, completed


def canonical_profile_env() -> dict[str, str]:
    values = {}
    for line in (CRATE / "marmotkit-release-profile.env").read_text().splitlines():
        if not line.startswith("export "):
            continue
        name, _, value = line[len("export ") :].partition("=")
        values[name] = value
    return values


def profile_env(base: dict, variant: str, strip: str) -> dict:
    env = dict(base)
    env.update(canonical_profile_env())
    chosen = BASELINE_PROFILE if variant == "baseline" else CANDIDATE_PROFILE
    # Source canonical settings, then override only the compared knobs.
    env["CARGO_PROFILE_RELEASE_LTO"] = "false" if chosen["lto"] is False else str(chosen["lto"])
    env["CARGO_PROFILE_RELEASE_CODEGEN_UNITS"] = str(chosen["codegen_units"])
    env["CARGO_PROFILE_RELEASE_STRIP"] = strip
    return env


def find_android_ndk() -> Path | None:
    for key in ("ANDROID_NDK_HOME", "ANDROID_NDK_ROOT", "NDK_HOME"):
        candidate = os.environ.get(key)
        if candidate and Path(candidate, "toolchains/llvm/prebuilt").is_dir():
            return Path(candidate)
    sdk_root = os.environ.get("ANDROID_HOME") or os.environ.get("ANDROID_SDK_ROOT")
    if sdk_root:
        ndk_root = Path(sdk_root) / "ndk"
        if ndk_root.is_dir():
            versions = sorted(path for path in ndk_root.iterdir() if path.is_dir())
            if versions:
                return versions[-1]
    return None


def android_host_tag(ndk: Path) -> str:
    prebuilt = ndk / "toolchains/llvm/prebuilt"
    if (prebuilt / "linux-x86_64").is_dir():
        return "linux-x86_64"
    if (prebuilt / "darwin-x86_64").is_dir():
        return "darwin-x86_64"
    hosts = sorted(path.name for path in prebuilt.iterdir() if path.is_dir())
    if not hosts:
        raise RuntimeError(f"no Android NDK host toolchain under {prebuilt}")
    return hosts[-1]


def configure_android_toolchain(env: dict, ndk: Path, triple: str) -> None:
    host_tag = android_host_tag(ndk)
    toolchain_bin = ndk / "toolchains/llvm/prebuilt" / host_tag / "bin"
    clang = toolchain_bin / f"{ANDROID_CLANG_PREFIX[triple]}{ANDROID_API}-clang"
    if not clang.is_file():
        raise RuntimeError(f"Android clang not found: {clang}")
    cargo_env = triple.upper().replace("-", "_")
    cc_env = triple.replace("-", "_")
    env[f"CARGO_TARGET_{cargo_env}_LINKER"] = str(clang)
    env[f"CARGO_TARGET_{cargo_env}_AR"] = str(toolchain_bin / "llvm-ar")
    env[f"CC_{cc_env}"] = str(clang)
    env[f"AR_{cc_env}"] = str(toolchain_bin / "llvm-ar")
    env[f"RANLIB_{cc_env}"] = str(toolchain_bin / "llvm-ranlib")


def android_reduction_failures(artifacts: list[dict]) -> list[str]:
    failures = []
    for row in artifacts:
        if row["target"] not in PRIMARY_ANDROID_TARGETS:
            continue
        if row["availability"] != "measured":
            failures.append(f"{row['target']} was not measured ({row.get('reason')})")
            continue
        delta = row.get("delta_bytes")
        if not isinstance(delta, int) or delta >= 0:
            failures.append(
                f"{row['target']} did not shrink: baseline={row['baseline_bytes']} "
                f"candidate={row['candidate_bytes']} delta={delta}"
            )
    return failures


def artifact_row(target, kind, strip, baseline, candidate, reason=None):
    row = {
        "target": target,
        "kind": kind,
        "baseline_bytes": None,
        "candidate_bytes": None,
        "baseline_sha256": None,
        "candidate_sha256": None,
        "delta_bytes": None,
        "delta_percent": None,
        "baseline_profile": {**BASELINE_PROFILE, "strip": strip},
        "candidate_profile": {**CANDIDATE_PROFILE, "strip": strip},
        "availability": "unavailable" if reason else "measured",
        "reason": reason,
    }
    if baseline and candidate and baseline.exists() and candidate.exists():
        row["baseline_bytes"] = baseline.stat().st_size
        row["candidate_bytes"] = candidate.stat().st_size
        row["baseline_sha256"] = sha256_file(baseline)
        row["candidate_sha256"] = sha256_file(candidate)
        row["delta_bytes"] = row["candidate_bytes"] - row["baseline_bytes"]
        if row["baseline_bytes"]:
            row["delta_percent"] = (row["delta_bytes"] / row["baseline_bytes"]) * 100
        row["availability"] = "measured"
        row["reason"] = None
    return row


def host_library(target_dir: Path) -> Path:
    if sys.platform == "darwin":
        return target_dir / "release" / "libmarmot_uniffi.dylib"
    return target_dir / "release" / "libmarmot_uniffi.so"


def rustc_has_target(triple: str) -> bool:
    listed = subprocess.run(
        ["rustup", "target", "list", "--installed"],
        check=False,
        capture_output=True,
        text=True,
    )
    return listed.returncode == 0 and triple in listed.stdout.splitlines()


def measure_library(workspace, env, log_dir, extra_args, name, features=None):
    command = [
        "cargo",
        "build",
        "--locked",
        "--release",
        "-p",
        "marmot-uniffi",
        *extra_args,
    ]
    selected = FEATURES if features is None else features
    if selected:
        command.extend(["--features", ",".join(selected)])
    duration, _ = run(command, env, workspace, log_dir, name)
    return duration


def collect_cpu(target_dir: Path):
    root = target_dir / "criterion"
    rows = []
    if not root.is_dir():
        return rows
    for estimates in root.rglob("estimates.json"):
        if estimates.parent.name != "new":
            continue
        data = json.loads(estimates.read_text())
        mean = data.get("mean", {})
        rows.append(
            {
                "benchmark": str(estimates.relative_to(root)),
                "units": "ns",
                "point_estimate": mean.get("point_estimate"),
                "confidence_interval": {
                    "lower": mean.get("confidence_interval", {}).get("lower_bound"),
                    "upper": mean.get("confidence_interval", {}).get("upper_bound"),
                },
                "raw_result": str(estimates),
            }
        )
    return rows


def render_markdown(report: dict) -> str:
    lines = [
        "# MarmotKit release-profile measurements",
        "",
        f"Schema: {report['schema_version']}",
        f"Source SHA: `{report['source_sha']}`",
        f"Builder SHA: `{report['builder_sha']}`",
        "",
        "| Target | Kind | Baseline bytes | Candidate bytes | Delta bytes | Delta % | Status |",
        "| --- | --- | ---: | ---: | ---: | ---: | --- |",
    ]
    for row in report["artifacts"]:
        lines.append(
            "| {target} | {kind} | {baseline_bytes} | {candidate_bytes} | {delta_bytes} | {delta} | {availability} |".format(
                target=row["target"],
                kind=row["kind"],
                baseline_bytes=row["baseline_bytes"] if row["baseline_bytes"] is not None else "unavailable",
                candidate_bytes=row["candidate_bytes"] if row["candidate_bytes"] is not None else "unavailable",
                delta_bytes=row["delta_bytes"] if row["delta_bytes"] is not None else "unavailable",
                delta=(
                    f"{row['delta_percent']:.2f}"
                    if isinstance(row.get("delta_percent"), (int, float))
                    else "unavailable"
                ),
                availability=row["availability"],
            )
        )
    lines.extend(["", "## CPU", ""])
    if not report["cpu_runs"]:
        lines.append("No CPU measurements were collected.")
    for row in report["cpu_runs"]:
        estimate = row.get("point_estimate")
        lines.append(
            f"- `{row['benchmark']}` ({row['variant']}): "
            f"{estimate if estimate is not None else 'unavailable'} {row['units']}"
        )
    lines.append("")
    return "\n".join(lines)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, default=ROOT)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--markdown", type=Path)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--builder-sha", required=True)
    parser.add_argument("--host", action="store_true")
    parser.add_argument("--android", action="store_true")
    parser.add_argument("--apple", action="store_true")
    parser.add_argument("--cpu", action="store_true")
    parser.add_argument(
        "--require-android-reduction",
        action="store_true",
        help="Fail when a primary ARM Android ABI is unmeasured or does not shrink",
    )
    args = parser.parse_args(argv)
    workspace = args.workspace.resolve()
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    work = (args.work_dir or (workspace / "target/release-profile-measure")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    log_dir = work / "logs"
    log_dir.mkdir(exist_ok=True)
    lock_sha = sha256_file(workspace / "Cargo.lock")
    rustc = subprocess.check_output(["rustc", "--version"], text=True).strip()
    cargo = subprocess.check_output(["cargo", "--version"], text=True).strip()
    commands = []
    artifacts = []
    cpu_runs = []
    base_env = os.environ.copy()
    base_env["CARGO_INCREMENTAL"] = "0"

    if args.host:
        for variant in ("baseline", "candidate"):
            target_dir = work / f"host-{variant}"
            env = profile_env(base_env, variant, "none")
            env["CARGO_TARGET_DIR"] = str(target_dir)
            duration = measure_library(
                workspace, env, log_dir, [], f"host-{variant}"
            )
            commands.append(
                {
                    "name": f"host-{variant}",
                    "compile_seconds": duration,
                    "target_dir": str(target_dir),
                }
            )
        artifacts.append(
            artifact_row(
                "host",
                "host_generation_library",
                "none",
                host_library(work / "host-baseline"),
                host_library(work / "host-candidate"),
            )
        )
        default_dir = work / "host-default-features"
        env = profile_env(base_env, "candidate", "none")
        env["CARGO_TARGET_DIR"] = str(default_dir)
        duration = measure_library(
            workspace, env, log_dir, [], "host-default-features-candidate", features=[]
        )
        commands.append(
            {
                "name": "host-default-features-candidate",
                "compile_seconds": duration,
                "target_dir": str(default_dir),
                "features": [],
            }
        )
        default_lib = host_library(default_dir)
        artifacts.append(
            {
                "target": "host",
                "kind": "host_generation_library_default_features",
                "baseline_bytes": None,
                "candidate_bytes": default_lib.stat().st_size if default_lib.exists() else None,
                "baseline_sha256": None,
                "candidate_sha256": sha256_file(default_lib) if default_lib.exists() else None,
                "delta_bytes": None,
                "delta_percent": None,
                "baseline_profile": None,
                "candidate_profile": {**CANDIDATE_PROFILE, "strip": "none"},
                "availability": "measured" if default_lib.exists() else "unavailable",
                "reason": None if default_lib.exists() else "default-feature host library missing",
            }
        )
    else:
        artifacts.append(
            artifact_row("host", "host_generation_library", "none", None, None, "not requested")
        )

    for abi, triple in ANDROID_ABIS.items():
        strip = "symbols"
        if not args.android:
            artifacts.append(
                artifact_row(triple, "android_jni_so", strip, None, None, "not requested")
            )
            continue
        ndk = find_android_ndk()
        if not rustc_has_target(triple) or ndk is None:
            artifacts.append(
                artifact_row(
                    triple,
                    "android_jni_so",
                    strip,
                    None,
                    None,
                    "Android NDK or Rust target unavailable",
                )
            )
            continue
        paths = {}
        for variant in ("baseline", "candidate"):
            target_dir = work / f"android-{variant}"
            env = profile_env(base_env, variant, strip)
            env["CARGO_TARGET_DIR"] = str(target_dir)
            configure_android_toolchain(env, ndk, triple)
            duration = measure_library(
                workspace,
                env,
                log_dir,
                ["--target", triple],
                f"android-{abi}-{variant}",
            )
            commands.append(
                {
                    "name": f"android-{abi}-{variant}",
                    "compile_seconds": duration,
                    "target_dir": str(target_dir),
                }
            )
            paths[variant] = target_dir / triple / "release" / "libmarmot_uniffi.so"
        artifacts.append(
            artifact_row(triple, "android_jni_so", strip, paths["baseline"], paths["candidate"])
        )

    for triple, kind in APPLE_SLICES.items():
        if not args.apple:
            artifacts.append(
                artifact_row(triple, kind, "none", None, None, "not requested")
            )
            continue
        if not rustc_has_target(triple):
            artifacts.append(
                artifact_row(triple, kind, "none", None, None, "Apple Rust target unavailable")
            )
            continue
        paths = {}
        for variant in ("baseline", "candidate"):
            target_dir = work / f"apple-{variant}"
            env = profile_env(base_env, variant, "none")
            env["CARGO_TARGET_DIR"] = str(target_dir)
            duration = measure_library(
                workspace,
                env,
                log_dir,
                ["--target", triple],
                f"apple-{triple}-{variant}",
            )
            commands.append(
                {
                    "name": f"apple-{triple}-{variant}",
                    "compile_seconds": duration,
                    "target_dir": str(target_dir),
                }
            )
            paths[variant] = target_dir / triple / "release" / "libmarmot_uniffi.a"
        artifacts.append(
            artifact_row(triple, kind, "none", paths["baseline"], paths["candidate"])
        )

    if args.cpu:
        for variant in ("baseline", "candidate"):
            target_dir = work / f"cpu-{variant}"
            env = profile_env(base_env, variant, "none")
            env["CARGO_TARGET_DIR"] = str(target_dir)
            command = [
                "cargo",
                "bench",
                "--locked",
                "--profile",
                "release",
                "-p",
                "cgka-engine",
                "--bench",
                "group_lifecycle",
                "--",
                "create_group/",
                "--sample-size",
                "10",
                "--warm-up-time",
                "1",
                "--measurement-time",
                "3",
                "--noplot",
            ]
            # Later groups in this file construct fixtures during registration.
            # A substring filter still executes those functions, so collect
            # create_group estimates even when a later fixture asserts.
            try:
                duration, _ = run(command, env, workspace, log_dir, f"cpu-{variant}")
                cpu_error = None
            except RuntimeError as error:
                duration = None
                cpu_error = str(error)
            commands.append(
                {
                    "name": f"cpu-{variant}",
                    "compile_seconds": duration,
                    "target_dir": str(target_dir),
                    "availability": "measured" if cpu_error is None else "partial",
                    "reason": cpu_error,
                }
            )
            rows = collect_cpu(target_dir)
            if rows:
                for row in rows:
                    row["variant"] = variant
                    row["profile"] = (
                        BASELINE_PROFILE if variant == "baseline" else CANDIDATE_PROFILE
                    )
                    if cpu_error:
                        row["collection_error"] = cpu_error
                    cpu_runs.append(row)
            else:
                cpu_runs.append(
                    {
                        "benchmark": "group_lifecycle/create_group",
                        "variant": variant,
                        "units": "ns",
                        "point_estimate": None,
                        "confidence_interval": None,
                        "raw_result": None,
                        "reason": cpu_error or "no criterion estimates",
                    }
                )
    else:
        cpu_runs.append(
            {
                "benchmark": "group_lifecycle/create_group",
                "variant": "unmeasured",
                "units": "ns",
                "point_estimate": None,
                "confidence_interval": None,
                "raw_result": None,
                "reason": "not requested",
            }
        )

    report = {
        "schema_version": 1,
        "source_sha": args.source_sha,
        "builder_sha": args.builder_sha,
        "lock_sha256": lock_sha,
        "toolchains": {
            "rustc": rustc,
            "cargo": cargo,
            "android_ndk_home": str(find_android_ndk() or "unavailable"),
            "android_api": ANDROID_API,
            "xcodebuild": shutil.which("xcodebuild") or "unavailable",
        },
        "features": FEATURES,
        "commands": commands,
        "artifacts": artifacts,
        "cpu_runs": cpu_runs,
    }
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    markdown = render_markdown(report)
    if args.markdown:
        args.markdown.write_text(markdown)
    else:
        sys.stdout.write(markdown)
    if args.require_android_reduction:
        failures = android_reduction_failures(artifacts)
        if failures:
            sys.stderr.write("Android reduction gate failed:\n")
            for failure in failures:
                sys.stderr.write(f"  {failure}\n")
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
