# PRNS esp-wifi-sys-esp32c6 vendor

- Package: `esp-wifi-sys-esp32c6 0.2.0`
- Upstream: `https://github.com/esp-rs/esp-wifi-sys` @ `fee9770fc96fa3bb753b2ce4bd968daa4f068a04`
- Blobs: ESP-IDF 5.5.3 generation
- Host ABI: `ext_version` / `config_version` **`0x20250825` / `0x20251211`**, config magic at offset **`0x74`**

## Status: stock blob (no controller overlay)

`libs/libble_app.a` is the **unchanged** crates.io 0.2.0 archive:

`3c5964b206aa9ae9843118d41812a477a7612414b7f03a3fce9f9aeb39774b32`

### Why IDF#16984 zips are not same-generation

The null-check library from [esp-idf#16984](https://github.com/espressif/esp-idf/issues/16984)
(`files/21509331`, controller hash `9b70ac9`) is a **different ABI**:

| Check | Stock / esp-radio 0.18 | Issue zip1 |
|-------|------------------------|------------|
| `esp_register_ext_funcs` `ext_version` | `0x20250825` | `0x20250415` |
| `r_ble_controller_init` `config_version` | `0x20251211` | `0x20250606` |
| Config magic field offset | `0x74` | `0x70` |

Patching only the version constants still fails because the controller config
struct layout differs. Official `esp32c6-bt-lib` at IDF v5.5.5 / HEAD keeps
`0x20250825` but still lacks the `lw`→`beqz`→`lhu` null guard.

A same-generation fix needs either:

1. An Espressif `libble_app.a` built for **`0x20250825` / magic@0x74`** that
   includes the null check, or
2. A verified binary patch of stock `.high_perf_code_iram1.5`
   `r_ble_lll_conn_txbuf_insert_after` (insert `c.beqz` before the faulting
   `lhu`) with all internal branches/relocs updated.

Until then, keep stock and mitigate in software (peripheral-only, softer
intervals, etc.).
