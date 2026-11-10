# Third-party licenses

RAST itself is MIT licensed (see [LICENSE](../LICENSE)). This file lists the third-party Rust crates compiled into the `rast` binary.

**Generated file — do not edit manually.** Regenerate with `scripts/generate-third-party-licenses.sh` after changing `Cargo.lock`.

The locked dependency graph (`Cargo.lock`) contains 323 third-party Rust crates, including crates used only for specific platforms or for tests; only the subset required by the Linux release build is linked into the binary. Each crate remains under the license declared by its authors; the full license texts are available in the upstream crate packages. Package-level license metadata follows the SPDX expressions reported by each crate.

Frontend build tooling (Vite, TypeScript and their transitive dependencies) is used only to build `frontend/dist` and is not distributed inside the binary; their exact versions and licenses are pinned in `frontend/package-lock.json`.

## License summary

| License expression | Packages |
| --- | ---: |
| MIT OR Apache-2.0 | 182 |
| MIT | 49 |
| Apache-2.0 OR MIT | 23 |
| Unicode-3.0 | 18 |
| MIT/Apache-2.0 | 8 |
| Zlib OR Apache-2.0 OR MIT | 6 |
| Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | 5 |
| Unlicense OR MIT | 5 |
| ISC | 4 |
| (MIT OR Apache-2.0) AND Unicode-3.0 | 3 |
| BSD-2-Clause OR Apache-2.0 OR MIT | 2 |
| MIT OR Apache-2.0 OR LGPL-2.1-or-later | 2 |
| Unlicense/MIT | 2 |
| Zlib | 2 |
| 0BSD OR MIT OR Apache-2.0 | 1 |
| Apache-2.0 | 1 |
| Apache-2.0 AND ISC | 1 |
| Apache-2.0 OR BSL-1.0 | 1 |
| Apache-2.0 OR BSL-1.0 OR MIT | 1 |
| Apache-2.0 OR ISC OR MIT | 1 |
| Apache-2.0/MIT | 1 |
| BSD-3-Clause | 1 |
| CC0-1.0 | 1 |
| CDLA-Permissive-2.0 | 1 |
| MIT AND BSD-3-Clause | 1 |
| MIT OR Zlib OR Apache-2.0 | 1 |

## Packages

| Package | Version | License | Source |
| --- | --- | --- | --- |
| `adler2` | 2.0.1 | 0BSD OR MIT OR Apache-2.0 | <https://github.com/oyvindln/adler2> |
| `aead` | 0.5.2 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/traits> |
| `aes` | 0.8.4 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/block-ciphers> |
| `aes-gcm` | 0.10.3 | Apache-2.0 OR MIT | <https://github.com/RustCrypto/AEADs> |
| `aho-corasick` | 1.1.5 | Unlicense OR MIT | <https://github.com/BurntSushi/aho-corasick> |
| `allocator-api2` | 0.2.21 | MIT OR Apache-2.0 | <https://github.com/zakarumych/allocator-api2> |
| `android_system_properties` | 0.1.6 | MIT OR Apache-2.0 | <https://github.com/nical/android_system_properties> |
| `anstream` | 1.0.0 | MIT OR Apache-2.0 | <https://github.com/rust-cli/anstyle.git> |
| `anstyle` | 1.0.14 | MIT OR Apache-2.0 | <https://github.com/rust-cli/anstyle.git> |
| `anstyle-parse` | 1.0.0 | MIT OR Apache-2.0 | <https://github.com/rust-cli/anstyle.git> |
| `anstyle-query` | 1.1.5 | MIT OR Apache-2.0 | <https://github.com/rust-cli/anstyle.git> |
| `anstyle-wincon` | 3.0.11 | MIT OR Apache-2.0 | <https://github.com/rust-cli/anstyle.git> |
| `argon2` | 0.6.0 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/password-hashes> |
| `async-compression` | 0.4.52 | MIT OR Apache-2.0 | <https://github.com/Nullus157/async-compression> |
| `async-trait` | 0.1.92 | MIT OR Apache-2.0 | <https://github.com/dtolnay/async-trait> |
| `atoi` | 2.0.0 | MIT | <https://github.com/pacman82/atoi-rs> |
| `atomic-waker` | 1.1.2 | Apache-2.0 OR MIT | <https://github.com/smol-rs/atomic-waker> |
| `autocfg` | 1.5.1 | Apache-2.0 OR MIT | <https://github.com/cuviper/autocfg> |
| `axum` | 0.8.9 | MIT | <https://github.com/tokio-rs/axum> |
| `axum-core` | 0.5.6 | MIT | <https://github.com/tokio-rs/axum> |
| `axum-macros` | 0.5.1 | MIT | <https://github.com/tokio-rs/axum> |
| `base64` | 0.22.1 | MIT OR Apache-2.0 | <https://github.com/marshallpierce/rust-base64> |
| `base64` | 0.23.1 | MIT OR Apache-2.0 | <https://github.com/marshallpierce/rust-base64> |
| `base64ct` | 1.8.3 | Apache-2.0 OR MIT | <https://github.com/RustCrypto/formats> |
| `bitflags` | 2.13.2 | MIT OR Apache-2.0 | <https://github.com/bitflags/bitflags> |
| `blake2` | 0.11.0 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/hashes> |
| `block-buffer` | 0.10.4 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/utils> |
| `block-buffer` | 0.12.1 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/utils> |
| `bstr` | 1.13.1 | MIT OR Apache-2.0 | <https://github.com/BurntSushi/bstr> |
| `bumpalo` | 3.20.3 | MIT OR Apache-2.0 | <https://github.com/fitzgen/bumpalo> |
| `byteorder` | 1.5.0 | Unlicense OR MIT | <https://github.com/BurntSushi/byteorder> |
| `bytes` | 1.12.1 | MIT | <https://github.com/tokio-rs/bytes> |
| `cc` | 1.11.1 | MIT OR Apache-2.0 | <https://github.com/rust-lang/cc-rs> |
| `cfg-if` | 1.0.5 | MIT OR Apache-2.0 | <https://github.com/rust-lang/cfg-if> |
| `chacha20` | 0.10.2 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/stream-ciphers> |
| `chrono` | 0.4.45 | MIT OR Apache-2.0 | <https://github.com/chronotope/chrono> |
| `cipher` | 0.4.4 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/traits> |
| `clap` | 4.6.7 | MIT OR Apache-2.0 | <https://github.com/clap-rs/clap> |
| `clap_builder` | 4.6.7 | MIT OR Apache-2.0 | <https://github.com/clap-rs/clap> |
| `clap_derive` | 4.6.7 | MIT OR Apache-2.0 | <https://github.com/clap-rs/clap> |
| `clap_lex` | 1.1.1 | MIT OR Apache-2.0 | <https://github.com/clap-rs/clap> |
| `cmov` | 0.5.4 | Apache-2.0 OR MIT | <https://github.com/RustCrypto/utils> |
| `colorchoice` | 1.0.5 | MIT OR Apache-2.0 | <https://github.com/rust-cli/anstyle.git> |
| `compression-codecs` | 0.4.47 | MIT OR Apache-2.0 | <https://github.com/Nullus157/async-compression> |
| `compression-core` | 0.4.34 | MIT OR Apache-2.0 | <https://github.com/Nullus157/async-compression> |
| `const-oid` | 0.10.2 | Apache-2.0 OR MIT | <https://github.com/RustCrypto/formats> |
| `core-foundation-sys` | 0.8.7 | MIT OR Apache-2.0 | <https://github.com/servo/core-foundation-rs> |
| `cpufeatures` | 0.2.17 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/utils> |
| `cpufeatures` | 0.3.1 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/utils> |
| `crc` | 3.4.0 | MIT OR Apache-2.0 | <https://github.com/mrhooray/crc-rs.git> |
| `crc-catalog` | 2.5.0 | MIT OR Apache-2.0 | <https://github.com/akhilles/crc-catalog.git> |
| `crc32fast` | 1.5.5 | MIT OR Apache-2.0 | <https://github.com/srijs/rust-crc32fast> |
| `crossbeam-queue` | 0.3.14 | MIT OR Apache-2.0 | <https://github.com/crossbeam-rs/crossbeam> |
| `crossbeam-utils` | 0.8.23 | MIT OR Apache-2.0 | <https://github.com/crossbeam-rs/crossbeam> |
| `crypto-common` | 0.1.7 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/traits> |
| `crypto-common` | 0.2.2 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/traits> |
| `ctr` | 0.9.2 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/block-modes> |
| `ctutils` | 0.4.8 | Apache-2.0 OR MIT | <https://github.com/RustCrypto/utils> |
| `digest` | 0.10.7 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/traits> |
| `digest` | 0.11.3 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/traits> |
| `dispatch2` | 0.3.1 | Zlib OR Apache-2.0 OR MIT | <https://github.com/madsmtm/objc2> |
| `displaydoc` | 0.2.7 | MIT OR Apache-2.0 | <https://github.com/yaahc/displaydoc> |
| `dotenvy` | 0.15.7 | MIT | <https://github.com/allan2/dotenvy> |
| `either` | 1.19.0 | MIT OR Apache-2.0 | <https://github.com/rayon-rs/either> |
| `equivalent` | 1.0.3 | Apache-2.0 OR MIT | <https://github.com/indexmap-rs/equivalent> |
| `errno` | 0.3.14 | MIT OR Apache-2.0 | <https://github.com/lambda-fairy/rust-errno> |
| `etcetera` | 0.11.0 | MIT OR Apache-2.0 | <https://github.com/lunacookies/etcetera> |
| `event-listener` | 5.4.2 | Apache-2.0 OR MIT | <https://github.com/smol-rs/event-listener> |
| `fastrand` | 2.5.0 | Apache-2.0 OR MIT | <https://github.com/smol-rs/fastrand> |
| `filetime` | 0.2.29 | MIT/Apache-2.0 | <https://github.com/alexcrichton/filetime> |
| `find-msvc-tools` | 0.1.15 | MIT OR Apache-2.0 | <https://github.com/rust-lang/cc-rs> |
| `flate2` | 1.1.11 | MIT OR Apache-2.0 | <https://github.com/rust-lang/flate2-rs> |
| `flume` | 0.12.0 | Apache-2.0/MIT | <https://github.com/zesterer/flume> |
| `foldhash` | 0.2.0 | Zlib | <https://github.com/orlp/foldhash> |
| `form_urlencoded` | 1.2.2 | MIT OR Apache-2.0 | <https://github.com/servo/rust-url> |
| `futures` | 0.3.34 | MIT OR Apache-2.0 | <https://github.com/rust-lang/futures-rs> |
| `futures-channel` | 0.3.34 | MIT OR Apache-2.0 | <https://github.com/rust-lang/futures-rs> |
| `futures-core` | 0.3.34 | MIT OR Apache-2.0 | <https://github.com/rust-lang/futures-rs> |
| `futures-executor` | 0.3.34 | MIT OR Apache-2.0 | <https://github.com/rust-lang/futures-rs> |
| `futures-intrusive` | 0.5.0 | MIT OR Apache-2.0 | <https://github.com/Matthias247/futures-intrusive> |
| `futures-io` | 0.3.34 | MIT OR Apache-2.0 | <https://github.com/rust-lang/futures-rs> |
| `futures-macro` | 0.3.34 | MIT OR Apache-2.0 | <https://github.com/rust-lang/futures-rs> |
| `futures-sink` | 0.3.34 | MIT OR Apache-2.0 | <https://github.com/rust-lang/futures-rs> |
| `futures-task` | 0.3.34 | MIT OR Apache-2.0 | <https://github.com/rust-lang/futures-rs> |
| `futures-util` | 0.3.34 | MIT OR Apache-2.0 | <https://github.com/rust-lang/futures-rs> |
| `generic-array` | 0.14.7 | MIT | <https://github.com/fizyk20/generic-array.git> |
| `getrandom` | 0.2.17 | MIT OR Apache-2.0 | <https://github.com/rust-random/getrandom> |
| `getrandom` | 0.3.4 | MIT OR Apache-2.0 | <https://github.com/rust-random/getrandom> |
| `getrandom` | 0.4.3 | MIT OR Apache-2.0 | <https://github.com/rust-random/getrandom> |
| `ghash` | 0.5.1 | Apache-2.0 OR MIT | <https://github.com/RustCrypto/universal-hashes> |
| `globset` | 0.4.21 | Unlicense OR MIT | <https://github.com/BurntSushi/ripgrep/tree/master/crates/globset> |
| `hashbrown` | 0.16.1 | MIT OR Apache-2.0 | <https://github.com/rust-lang/hashbrown> |
| `hashbrown` | 0.17.1 | MIT OR Apache-2.0 | <https://github.com/rust-lang/hashbrown> |
| `hashlink` | 0.11.1 | MIT OR Apache-2.0 | <https://github.com/djc/hashlink> |
| `heck` | 0.5.0 | MIT OR Apache-2.0 | <https://github.com/withoutboats/heck> |
| `hex` | 0.4.3 | MIT OR Apache-2.0 | <https://github.com/KokaKiwi/rust-hex> |
| `hkdf` | 0.13.0 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/KDFs> |
| `hmac` | 0.12.1 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/MACs> |
| `hmac` | 0.13.0 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/MACs> |
| `http` | 1.5.0 | MIT OR Apache-2.0 | <https://github.com/hyperium/http> |
| `http-body` | 1.1.0 | MIT | <https://github.com/hyperium/http-body> |
| `http-body-util` | 0.1.5 | MIT | <https://github.com/hyperium/http-body> |
| `httparse` | 1.10.1 | MIT OR Apache-2.0 | <https://github.com/seanmonstar/httparse> |
| `httpdate` | 1.0.3 | MIT OR Apache-2.0 | <https://github.com/pyfisch/httpdate> |
| `hybrid-array` | 0.4.15 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/hybrid-array> |
| `hyper` | 1.12.0 | MIT | <https://github.com/hyperium/hyper> |
| `hyper-util` | 0.1.21 | MIT | <https://github.com/hyperium/hyper-util> |
| `iana-time-zone` | 0.1.65 | MIT OR Apache-2.0 | <https://github.com/strawlab/iana-time-zone> |
| `iana-time-zone-haiku` | 0.1.2 | MIT OR Apache-2.0 | <https://github.com/strawlab/iana-time-zone> |
| `icu_collections` | 2.4.0 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `icu_locale_core` | 2.4.0 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `icu_normalizer` | 2.4.0 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `icu_normalizer_data` | 2.4.0 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `icu_properties` | 2.4.0 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `icu_properties_data` | 2.4.0 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `icu_provider` | 2.4.0 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `idna` | 1.1.0 | MIT OR Apache-2.0 | <https://github.com/servo/rust-url> |
| `idna_adapter` | 1.2.2 | Apache-2.0 OR MIT | <https://github.com/hsivonen/idna_adapter> |
| `indexmap` | 2.14.2 | Apache-2.0 OR MIT | <https://github.com/indexmap-rs/indexmap> |
| `inotify` | 0.11.5 | ISC | <https://github.com/hannobraun/inotify-rs> |
| `inotify-sys` | 0.1.8 | ISC | <https://github.com/hannobraun/inotify-sys> |
| `inout` | 0.1.4 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/utils> |
| `ipnet` | 2.12.2 | MIT OR Apache-2.0 | <https://github.com/krisprice/ipnet> |
| `is_terminal_polyfill` | 1.70.2 | MIT OR Apache-2.0 | <https://github.com/polyfill-rs/is_terminal_polyfill> |
| `itoa` | 1.0.19 | MIT OR Apache-2.0 | <https://github.com/dtolnay/itoa> |
| `js-sys` | 0.3.107 | MIT OR Apache-2.0 | <https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/js-sys> |
| `kqueue` | 1.2.1 | MIT | <https://crates.io/crates/kqueue/1.2.1> |
| `kqueue-sys` | 1.1.2 | MIT | <https://crates.io/crates/kqueue-sys/1.1.2> |
| `lazy_static` | 1.5.1 | MIT OR Apache-2.0 | <https://github.com/rust-lang-nursery/lazy-static.rs> |
| `libc` | 0.2.191 | MIT OR Apache-2.0 | <https://github.com/rust-lang/libc> |
| `libsqlite3-sys` | 0.37.0 | MIT | <https://github.com/rusqlite/rusqlite> |
| `linux-raw-sys` | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | <https://github.com/sunfishcode/linux-raw-sys> |
| `litemap` | 0.8.3 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `lock_api` | 0.4.14 | MIT OR Apache-2.0 | <https://github.com/Amanieu/parking_lot> |
| `log` | 0.4.35 | MIT OR Apache-2.0 | <https://github.com/rust-lang/log> |
| `matchers` | 0.2.0 | MIT | <https://github.com/hawkw/matchers> |
| `matchit` | 0.8.4 | MIT AND BSD-3-Clause | <https://github.com/ibraheemdev/matchit> |
| `md-5` | 0.11.0 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/hashes> |
| `memchr` | 2.8.3 | Unlicense OR MIT | <https://github.com/BurntSushi/memchr> |
| `mime` | 0.3.18 | MIT OR Apache-2.0 | <https://github.com/hyperium/mime> |
| `mime_guess` | 2.0.5 | MIT | <https://github.com/abonander/mime_guess> |
| `miniz_oxide` | 0.9.1 | MIT OR Zlib OR Apache-2.0 | <https://github.com/Frommi/miniz_oxide/tree/master/miniz_oxide> |
| `mio` | 1.2.4 | MIT | <https://github.com/tokio-rs/mio> |
| `notify` | 8.2.2 | CC0-1.0 | <https://github.com/notify-rs/notify.git> |
| `notify-types` | 2.3.0 | MIT OR Apache-2.0 | <https://github.com/notify-rs/notify.git> |
| `ntapi` | 0.4.3 | Apache-2.0 OR MIT | <https://github.com/MSxDOS/ntapi> |
| `nu-ansi-term` | 0.50.3 | MIT | <https://github.com/nushell/nu-ansi-term> |
| `num-traits` | 0.2.19 | MIT OR Apache-2.0 | <https://github.com/rust-num/num-traits> |
| `objc2` | 0.6.5 | MIT | <https://github.com/madsmtm/objc2> |
| `objc2-core-foundation` | 0.3.2 | Zlib OR Apache-2.0 OR MIT | <https://github.com/madsmtm/objc2> |
| `objc2-core-services` | 0.3.2 | Zlib OR Apache-2.0 OR MIT | <https://github.com/madsmtm/objc2> |
| `objc2-encode` | 4.1.0 | MIT | <https://github.com/madsmtm/objc2> |
| `objc2-foundation` | 0.3.2 | MIT | <https://github.com/madsmtm/objc2> |
| `objc2-io-kit` | 0.3.2 | Zlib OR Apache-2.0 OR MIT | <https://github.com/madsmtm/objc2> |
| `objc2-open-directory` | 0.3.2 | Zlib OR Apache-2.0 OR MIT | <https://github.com/madsmtm/objc2> |
| `once_cell` | 1.21.4 | MIT OR Apache-2.0 | <https://github.com/matklad/once_cell> |
| `once_cell_polyfill` | 1.70.2 | MIT OR Apache-2.0 | <https://github.com/polyfill-rs/once_cell_polyfill> |
| `opaque-debug` | 0.3.1 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/utils> |
| `parking` | 2.2.1 | Apache-2.0 OR MIT | <https://github.com/smol-rs/parking> |
| `parking_lot` | 0.12.5 | MIT OR Apache-2.0 | <https://github.com/Amanieu/parking_lot> |
| `parking_lot_core` | 0.9.12 | MIT OR Apache-2.0 | <https://github.com/Amanieu/parking_lot> |
| `password-hash` | 0.6.1 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/traits> |
| `percent-encoding` | 2.3.2 | MIT OR Apache-2.0 | <https://github.com/servo/rust-url> |
| `phc` | 0.6.1 | Apache-2.0 OR MIT | <https://github.com/RustCrypto/formats> |
| `pin-project-lite` | 0.2.17 | Apache-2.0 OR MIT | <https://github.com/taiki-e/pin-project-lite> |
| `pkg-config` | 0.3.34 | MIT OR Apache-2.0 | <https://github.com/rust-lang/pkg-config-rs> |
| `polyval` | 0.6.2 | Apache-2.0 OR MIT | <https://github.com/RustCrypto/universal-hashes> |
| `potential_utf` | 0.1.6 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `ppv-lite86` | 0.2.21 | MIT OR Apache-2.0 | <https://github.com/cryptocorrosion/cryptocorrosion> |
| `proc-macro2` | 1.0.107 | MIT OR Apache-2.0 | <https://github.com/dtolnay/proc-macro2> |
| `quote` | 1.0.47 | MIT OR Apache-2.0 | <https://github.com/dtolnay/quote> |
| `r-efi` | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | <https://github.com/r-efi/r-efi> |
| `r-efi` | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | <https://github.com/r-efi/r-efi> |
| `rand` | 0.10.3 | MIT OR Apache-2.0 | <https://github.com/rust-random/rand> |
| `rand` | 0.9.5 | MIT OR Apache-2.0 | <https://github.com/rust-random/rand> |
| `rand_chacha` | 0.9.0 | MIT OR Apache-2.0 | <https://github.com/rust-random/rand> |
| `rand_core` | 0.10.1 | MIT OR Apache-2.0 | <https://github.com/rust-random/rand_core> |
| `rand_core` | 0.6.4 | MIT OR Apache-2.0 | <https://github.com/rust-random/rand> |
| `rand_core` | 0.9.5 | MIT OR Apache-2.0 | <https://github.com/rust-random/rand> |
| `redox_syscall` | 0.5.18 | MIT | <https://crates.io/crates/redox_syscall/0.5.18> |
| `regex-automata` | 0.4.18 | MIT OR Apache-2.0 | <https://github.com/rust-lang/regex> |
| `regex-syntax` | 0.8.11 | MIT OR Apache-2.0 | <https://github.com/rust-lang/regex> |
| `ring` | 0.17.14 | Apache-2.0 AND ISC | <https://github.com/briansmith/ring> |
| `rust-embed` | 8.13.0 | MIT | <https://crates.io/crates/rust-embed/8.13.0> |
| `rust-embed-impl` | 8.13.0 | MIT | <https://crates.io/crates/rust-embed-impl/8.13.0> |
| `rust-embed-utils` | 8.13.0 | MIT | <https://crates.io/crates/rust-embed-utils/8.13.0> |
| `rustix` | 1.1.6 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | <https://github.com/bytecodealliance/rustix> |
| `rustls` | 0.23.46 | Apache-2.0 OR ISC OR MIT | <https://github.com/rustls/rustls> |
| `rustls-pki-types` | 1.15.1 | MIT OR Apache-2.0 | <https://github.com/rustls/pki-types> |
| `rustls-webpki` | 0.103.16 | ISC | <https://github.com/rustls/webpki> |
| `rustversion` | 1.0.23 | MIT OR Apache-2.0 | <https://github.com/dtolnay/rustversion> |
| `ryu` | 1.0.23 | Apache-2.0 OR BSL-1.0 | <https://github.com/dtolnay/ryu> |
| `same-file` | 1.0.6 | Unlicense/MIT | <https://github.com/BurntSushi/same-file> |
| `scopeguard` | 1.2.0 | MIT OR Apache-2.0 | <https://github.com/bluss/scopeguard> |
| `serde` | 1.0.229 | MIT OR Apache-2.0 | <https://github.com/serde-rs/serde> |
| `serde_core` | 1.0.229 | MIT OR Apache-2.0 | <https://github.com/serde-rs/serde> |
| `serde_derive` | 1.0.229 | MIT OR Apache-2.0 | <https://github.com/serde-rs/serde> |
| `serde_json` | 1.0.154 | MIT OR Apache-2.0 | <https://github.com/serde-rs/json> |
| `serde_path_to_error` | 0.1.20 | MIT OR Apache-2.0 | <https://github.com/dtolnay/path-to-error> |
| `serde_spanned` | 1.1.3 | MIT OR Apache-2.0 | <https://github.com/toml-rs/toml> |
| `serde_urlencoded` | 0.7.1 | MIT/Apache-2.0 | <https://github.com/nox/serde_urlencoded> |
| `sha1` | 0.11.0 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/hashes> |
| `sha2` | 0.10.9 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/hashes> |
| `sha2` | 0.11.1 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/hashes> |
| `sharded-slab` | 0.1.7 | MIT | <https://github.com/hawkw/sharded-slab> |
| `shlex` | 2.0.1 | MIT OR Apache-2.0 | <https://github.com/comex/rust-shlex> |
| `signal-hook-registry` | 1.4.8 | MIT OR Apache-2.0 | <https://github.com/vorner/signal-hook> |
| `simd-adler32` | 0.3.10 | MIT | <https://github.com/mcountryman/simd-adler32> |
| `slab` | 0.4.12 | MIT | <https://github.com/tokio-rs/slab> |
| `smallvec` | 1.16.5 | MIT OR Apache-2.0 | <https://github.com/servo/rust-smallvec> |
| `socket2` | 0.6.5 | MIT OR Apache-2.0 | <https://github.com/rust-lang/socket2> |
| `spin` | 0.9.9 | MIT | <https://github.com/mvdnes/spin-rs.git> |
| `sqlx` | 0.9.0 | MIT OR Apache-2.0 | <https://github.com/launchbadge/sqlx> |
| `sqlx-core` | 0.9.0 | MIT OR Apache-2.0 | <https://github.com/launchbadge/sqlx> |
| `sqlx-macros` | 0.9.0 | MIT OR Apache-2.0 | <https://github.com/launchbadge/sqlx> |
| `sqlx-macros-core` | 0.9.0 | MIT OR Apache-2.0 | <https://github.com/launchbadge/sqlx> |
| `sqlx-mysql` | 0.9.0 | MIT OR Apache-2.0 | <https://github.com/launchbadge/sqlx> |
| `sqlx-postgres` | 0.9.0 | MIT OR Apache-2.0 | <https://github.com/launchbadge/sqlx> |
| `sqlx-sqlite` | 0.9.0 | MIT OR Apache-2.0 | <https://github.com/launchbadge/sqlx> |
| `stable_deref_trait` | 1.2.1 | MIT OR Apache-2.0 | <https://github.com/storyyeller/stable_deref_trait> |
| `stringprep` | 0.1.5 | MIT/Apache-2.0 | <https://github.com/sfackler/rust-stringprep> |
| `strsim` | 0.11.1 | MIT | <https://github.com/rapidfuzz/strsim-rs> |
| `subtle` | 2.6.1 | BSD-3-Clause | <https://github.com/dalek-cryptography/subtle> |
| `syn` | 2.0.119 | MIT OR Apache-2.0 | <https://github.com/dtolnay/syn> |
| `syn` | 3.0.10 | MIT OR Apache-2.0 | <https://github.com/dtolnay/syn> |
| `sync_wrapper` | 1.0.2 | Apache-2.0 | <https://github.com/Actyx/sync_wrapper> |
| `synstructure` | 0.14.0 | MIT | <https://github.com/mystor/synstructure> |
| `sysinfo` | 0.40.1 | MIT | <https://github.com/GuillaumeGomez/sysinfo> |
| `tar` | 0.4.46 | MIT OR Apache-2.0 | <https://github.com/composefs/tar-rs> |
| `tempfile` | 3.27.0 | MIT OR Apache-2.0 | <https://github.com/Stebalien/tempfile> |
| `thiserror` | 2.0.21 | MIT OR Apache-2.0 | <https://github.com/dtolnay/thiserror> |
| `thiserror-impl` | 2.0.21 | MIT OR Apache-2.0 | <https://github.com/dtolnay/thiserror> |
| `thread_local` | 1.1.10 | MIT OR Apache-2.0 | <https://github.com/Amanieu/thread_local-rs> |
| `tinystr` | 0.8.5 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `tinyvec` | 1.13.3 | Zlib OR Apache-2.0 OR MIT | <https://github.com/Lokathor/tinyvec> |
| `tokio` | 1.54.1 | MIT | <https://github.com/tokio-rs/tokio> |
| `tokio-macros` | 2.7.2 | MIT | <https://github.com/tokio-rs/tokio> |
| `tokio-stream` | 0.1.19 | MIT | <https://github.com/tokio-rs/tokio> |
| `tokio-util` | 0.7.20 | MIT | <https://github.com/tokio-rs/tokio> |
| `toml` | 0.9.12+spec-1.1.0 | MIT OR Apache-2.0 | <https://github.com/toml-rs/toml> |
| `toml_datetime` | 0.7.5+spec-1.1.0 | MIT OR Apache-2.0 | <https://github.com/toml-rs/toml> |
| `toml_parser` | 1.1.5+spec-1.1.0 | MIT OR Apache-2.0 | <https://github.com/toml-rs/toml> |
| `toml_writer` | 1.2.0+spec-1.1.0 | MIT OR Apache-2.0 | <https://github.com/toml-rs/toml> |
| `tower` | 0.5.3 | MIT | <https://github.com/tower-rs/tower> |
| `tower-http` | 0.7.1 | MIT | <https://github.com/tower-rs/tower-http> |
| `tower-layer` | 0.3.3 | MIT | <https://github.com/tower-rs/tower> |
| `tower-service` | 0.3.3 | MIT | <https://github.com/tower-rs/tower> |
| `tracing` | 0.1.44 | MIT | <https://github.com/tokio-rs/tracing> |
| `tracing-attributes` | 0.1.31 | MIT | <https://github.com/tokio-rs/tracing> |
| `tracing-core` | 0.1.36 | MIT | <https://github.com/tokio-rs/tracing> |
| `tracing-log` | 0.2.0 | MIT | <https://github.com/tokio-rs/tracing> |
| `tracing-subscriber` | 0.3.23 | MIT | <https://github.com/tokio-rs/tracing> |
| `typenum` | 1.20.1 | MIT OR Apache-2.0 | <https://github.com/paholg/typenum> |
| `unicase` | 2.10.0 | MIT OR Apache-2.0 | <https://github.com/seanmonstar/unicase> |
| `unicode-bidi` | 0.3.18 | MIT OR Apache-2.0 | <https://github.com/servo/unicode-bidi> |
| `unicode-ident` | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 | <https://github.com/dtolnay/unicode-ident> |
| `unicode-normalization` | 0.1.26 | (MIT OR Apache-2.0) AND Unicode-3.0 | <https://github.com/unicode-rs/unicode-normalization> |
| `unicode-properties` | 0.1.5 | (MIT OR Apache-2.0) AND Unicode-3.0 | <https://github.com/unicode-rs/unicode-properties> |
| `universal-hash` | 0.5.1 | MIT OR Apache-2.0 | <https://github.com/RustCrypto/traits> |
| `untrusted` | 0.9.0 | ISC | <https://github.com/briansmith/untrusted> |
| `ureq` | 3.5.1 | MIT OR Apache-2.0 | <https://github.com/algesten/ureq> |
| `ureq-proto` | 0.7.1 | MIT OR Apache-2.0 | <https://github.com/algesten/ureq-proto> |
| `url` | 2.5.8 | MIT OR Apache-2.0 | <https://github.com/servo/rust-url> |
| `utf8-zero` | 0.8.1 | MIT OR Apache-2.0 | <https://github.com/algesten/utf8-zero> |
| `utf8_iter` | 1.1.0 | Apache-2.0 OR MIT | <https://github.com/hsivonen/utf8_iter> |
| `utf8parse` | 0.2.2 | Apache-2.0 OR MIT | <https://github.com/alacritty/vte> |
| `utf_types` | 1.0.0 | Apache-2.0 OR MIT | <https://github.com/hsivonen/utf_types> |
| `uuid` | 1.29.2 | Apache-2.0 OR MIT | <https://github.com/uuid-rs/uuid> |
| `valuable` | 0.1.1 | MIT | <https://github.com/tokio-rs/valuable> |
| `vcpkg` | 0.2.15 | MIT/Apache-2.0 | <https://github.com/mcgoo/vcpkg-rs> |
| `version_check` | 0.9.5 | MIT/Apache-2.0 | <https://github.com/SergioBenitez/version_check> |
| `walkdir` | 2.5.0 | Unlicense/MIT | <https://github.com/BurntSushi/walkdir> |
| `wasi` | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | <https://github.com/bytecodealliance/wasi> |
| `wasip2` | 1.0.4+wasi-0.2.12 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | <https://github.com/bytecodealliance/wasi-rs> |
| `wasm-bindgen` | 0.2.130 | MIT OR Apache-2.0 | <https://github.com/wasm-bindgen/wasm-bindgen> |
| `wasm-bindgen-macro` | 0.2.130 | MIT OR Apache-2.0 | <https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/macro> |
| `wasm-bindgen-macro-support` | 0.2.130 | MIT OR Apache-2.0 | <https://github.com/wasm-bindgen/wasm-bindgen/tree/main/crates/macro-support> |
| `wasm-bindgen-shared` | 0.2.130 | MIT OR Apache-2.0 | <https://github.com/wasm-bindgen/wasm-bindgen/tree/master/crates/shared> |
| `webpki-roots` | 1.0.11 | CDLA-Permissive-2.0 | <https://github.com/rustls/webpki-roots> |
| `whoami` | 2.1.3 | Apache-2.0 OR BSL-1.0 OR MIT | <https://github.com/ardaku/whoami> |
| `winapi` | 0.3.9 | MIT/Apache-2.0 | <https://github.com/retep998/winapi-rs> |
| `winapi-i686-pc-windows-gnu` | 0.4.0 | MIT/Apache-2.0 | <https://github.com/retep998/winapi-rs> |
| `winapi-util` | 0.1.11 | Unlicense OR MIT | <https://github.com/BurntSushi/winapi-util> |
| `winapi-x86_64-pc-windows-gnu` | 0.4.0 | MIT/Apache-2.0 | <https://github.com/retep998/winapi-rs> |
| `windows` | 0.62.2 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-collections` | 0.3.2 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-core` | 0.62.2 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-future` | 0.3.2 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-implement` | 0.60.2 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-interface` | 0.59.3 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-link` | 0.2.1 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-numerics` | 0.3.1 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-result` | 0.4.1 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-strings` | 0.5.1 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-sys` | 0.52.0 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-sys` | 0.61.2 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-targets` | 0.52.6 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows-threading` | 0.2.1 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows_aarch64_gnullvm` | 0.52.6 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows_aarch64_msvc` | 0.52.6 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows_i686_gnu` | 0.52.6 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows_i686_gnullvm` | 0.52.6 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows_i686_msvc` | 0.52.6 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows_x86_64_gnu` | 0.52.6 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows_x86_64_gnullvm` | 0.52.6 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `windows_x86_64_msvc` | 0.52.6 | MIT OR Apache-2.0 | <https://github.com/microsoft/windows-rs> |
| `winnow` | 0.7.15 | MIT | <https://github.com/winnow-rs/winnow> |
| `winnow` | 1.0.5 | MIT | <https://github.com/winnow-rs/winnow> |
| `wit-bindgen` | 0.57.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | <https://github.com/bytecodealliance/wit-bindgen> |
| `writeable` | 0.6.4 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `xattr` | 1.7.0 | MIT OR Apache-2.0 | <https://github.com/Stebalien/xattr> |
| `yoke` | 0.8.4 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `yoke-derive` | 0.8.4 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `zerocopy` | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | <https://github.com/google/zerocopy> |
| `zerocopy-derive` | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT | <https://github.com/google/zerocopy> |
| `zerofrom` | 0.1.8 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `zerofrom-derive` | 0.1.8 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `zeroize` | 1.9.1 | Apache-2.0 OR MIT | <https://github.com/RustCrypto/utils> |
| `zerotrie` | 0.2.5 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `zerovec` | 0.11.9 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `zerovec-derive` | 0.11.7 | Unicode-3.0 | <https://github.com/unicode-org/icu4x> |
| `zlib-rs` | 0.6.8 | Zlib | <https://github.com/trifectatechfoundation/zlib-rs> |
| `zmij` | 1.0.23 | MIT | <https://github.com/dtolnay/zmij> |
