# Personal Hopspot — Dioxus Android demo

Touch-first management UI for the same Hopspot *functionality* as the OLED /
JNI face: interface status, announce, limits, sleep, power toggles, and RNS
config export. This first cut uses **mock node state** so the UI can iterate
without binding to `PrnsService`.

It is a separate Cargo workspace (like `docs/website`) so Dioxus stays off the
suite `no_std` / lock gates.

## Run (web preview)

Useful while iterating on layout without an emulator:

```bash
cd personal-hopspot/mobile/dioxus-android
dx serve --platform web
```

Requires `dioxus-cli` **0.7.5** (same pin as the public site).

## Run (Android)

```bash
cd personal-hopspot/mobile/dioxus-android
dx serve --platform android --features mobile --no-default-features
```

Needs Android Studio / SDK / NDK and a running emulator or device. The `mobile`
feature enables `dioxus/mobile`; default builds use `dioxus/web` for the preview.

## What is mocked vs live later

| Surface | Demo now | Later |
|---|---|---|
| Engine status / traffic | Static sample | `PrnsService` status bundle |
| Interface list / detail | Sample cards | `interface_snapshots` |
| Announce / Sleep / Power | Local signal updates + toast | Existing `UiAction` / JNI |
| RNS Config | Sample template + toast | Live `sideband_join_config()` + clipboard |

## Not in scope for this demo

- Pixel clone of the OLED face
- LoRa tuner (can be a follow-up screen)
- Replacing the shipping `org.personal.hopspot` APK
