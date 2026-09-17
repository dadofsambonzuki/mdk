# MarmotKit release-profile measurements

Schema: 1
Source SHA: `b9c305c0a67f59527f7291d2f69e4d23707c6cb2`
Builder SHA: `b9c305c0a67f59527f7291d2f69e4d23707c6cb2`
Toolchains: rustc 1.97.1 (8bab26f4f 2026-07-14), cargo 1.97.1 (c980f4866 2026-06-30)
Features: `otlp-export,product-analytics-export` for the primary host comparison
Compared profiles: baseline `lto=false,codegen-units=16` vs candidate `lto=thin,codegen-units=1`
Strip: `none` for host/Apple, `symbols` for Android (both variants)

Host libraries are uncompressed `libmarmot_uniffi.so` bytes. Android and Apple
slices are unavailable on this Linux builder (no NDK, no Apple targets). The
non-publishing `bindings-profile.yml` workflow collects those on native runners
and fails the Linux lane if a primary ARM Android ABI does not shrink. Linux CI
on `b9c305c0a67f59527f7291d2f69e4d23707c6cb2` passed that Android reduction gate;
the macOS Apple slice job on the same head failed and did not retain diagnostics.

| Target | Kind | Baseline bytes | Candidate bytes | Delta bytes | Delta % | Status |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| host | host_generation_library | 89237400 | 66959200 | -22278200 | -24.97 | measured |
| host | host_generation_library_default_features | unavailable | 66524864 | unavailable | unavailable | measured (candidate smoke) |
| aarch64-linux-android | android_jni_so | unavailable | unavailable | unavailable | unavailable | unavailable (no NDK) |
| armv7-linux-androideabi | android_jni_so | unavailable | unavailable | unavailable | unavailable | unavailable (no NDK) |
| i686-linux-android | android_jni_so | unavailable | unavailable | unavailable | unavailable | unavailable (no NDK) |
| x86_64-linux-android | android_jni_so | unavailable | unavailable | unavailable | unavailable | unavailable (no NDK) |
| aarch64-apple-ios | apple_static_archive | unavailable | unavailable | unavailable | unavailable | unavailable (no Apple target) |
| aarch64-apple-ios-sim | apple_static_archive | unavailable | unavailable | unavailable | unavailable | unavailable (no Apple target) |
| aarch64-apple-darwin | apple_static_archive | unavailable | unavailable | unavailable | unavailable | unavailable (no Apple target) |

## CPU (`group_lifecycle` / `create_group`, `--profile release`)

Fresh Criterion estimates from successful scoped invocations (`MDK_RELEASE_PROFILE_CPU_ONLY=1`).
Candidate is slightly faster on every collected row; there is no >5% regression to investigate.

| Benchmark | Baseline ns | Candidate ns | Delta % |
| --- | ---: | ---: | ---: |
| create_group/1 invitees, retention disabled | 3222687 | 3121102 | -3.15 |
| create_group/1 invitees, retention enabled | 3241634 | 3118766 | -3.79 |
| create_group/32 invitees, retention disabled | 24518834 | 23911779 | -2.48 |
| create_group/32 invitees, retention enabled | 24609446 | 24148020 | -1.87 |
| create_group/8 invitees, retention disabled | 7711900 | 7513005 | -2.58 |
| create_group/8 invitees, retention enabled | 7669328 | 7531236 | -1.80 |

Host candidate library SHA-256: `35785a19268f1dc9aad49aa806a284b89535ccd256503c3b18f3871c0096ce56`
Host baseline library SHA-256: `52d16996b58bf0e9fa0d9ac769bbbfef894dcf0df6b09e600d304622652a16bf`
