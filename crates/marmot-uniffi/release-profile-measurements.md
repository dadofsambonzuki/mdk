# MarmotKit release-profile measurements

Schema: 1
Source SHA: `c41404a02acd9c3e020e26de4cd13895477288ec`
Builder SHA: `c41404a02acd9c3e020e26de4cd13895477288ec`
Toolchains: rustc 1.97.1, cargo 1.97.1
Features: `otlp-export,product-analytics-export` for the primary host comparison
Compared profiles: baseline `lto=false,codegen-units=16` vs candidate `lto=thin,codegen-units=1`
Strip: `none` for host/Apple, `symbols` for Android (both variants)

Host libraries are uncompressed `libmarmot_uniffi.so` bytes. Android and Apple
slices are unavailable on this Linux builder (no NDK, no Apple targets). The
non-publishing `bindings-profile.yml` workflow collects those on native runners
and fails the Linux lane if a primary ARM Android ABI does not shrink.

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

Later benches in that file construct fixtures during registration, so the process
can still fail after `create_group` estimates are written. The numbers below are
those Criterion estimates. Candidate is slightly faster on every collected row;
there is no >5% regression to investigate.

| Benchmark | Baseline ns | Candidate ns | Delta % |
| --- | ---: | ---: | ---: |
| create_group/1 invitees, retention disabled | 3183918 | 3138801 | -1.42 |
| create_group/1 invitees, retention enabled | 3177839 | 3119295 | -1.84 |
| create_group/8 invitees, retention disabled | 7783528 | 7414871 | -4.74 |
| create_group/8 invitees, retention enabled | 7685393 | 7479111 | -2.68 |
| create_group/32 invitees, retention disabled | 24198428 | 23580561 | -2.55 |
| create_group/32 invitees, retention enabled | 24299296 | 23643619 | -2.70 |

Host candidate library SHA-256: `35785a19268f1dc9aad49aa806a284b89535ccd256503c3b18f3871c0096ce56`
Host baseline library SHA-256: `52d16996b58bf0e9fa0d9ac769bbbfef894dcf0df6b09e600d304622652a16bf`
