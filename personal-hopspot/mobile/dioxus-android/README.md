# Personal Hopspot — Dioxus Android demo

Touch-first management UI for the same Hopspot *functionality* as the OLED /
JNI face: interface status, announce, limits, sleep, power toggles, and RNS
config export.

- **Web preview** (`dx serve --platform web`): mock node state for layout work
- **Hopspot APK** (`dioxus` flavor): live `PrnsService` via `HopspotBridge`
  (same process/architecture as the OLED face; closing the UI does not stop
  the engine)

It is a separate Cargo workspace (like `docs/website`) so Dioxus stays off the
suite `no_std` / lock gates.

## Run (web preview)

```bash
cd personal-hopspot/mobile/dioxus-android
dx serve --platform web
```

Requires `dioxus-cli` **0.7.5** (same pin as the public site).

## Live in the Hopspot APK

1. Sync the web build into Android assets:

```bash
personal-hopspot/mobile/scripts/sync-dioxus-assets.sh
```

2. Build native + APK (from `personal-hopspot/mobile/android`):

```bash
(cd rust && cargo ndk -t arm64-v8a -o ../app/src/main/jniLibs build --release)
./gradlew :app:assembleDioxusDebug
```

OLED regression flavor:

```bash
./gradlew :app:assembleOledDebug
```

The Activity only binds `PrnsService`; the foreground service owns the engine.

## Android NDK for standalone `dx serve --platform android`

Standalone `dx` Android is **not** the live target (no `PrnsService`). For
experiments, set:

```bash
export ANDROID_HOME="/opt/homebrew/share/android-commandlinetools"
export ANDROID_SDK_ROOT="$ANDROID_HOME"
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/27.2.12479018"
export NDK_HOME="$ANDROID_NDK_HOME"
```

## What is mocked vs live

| Surface | Web preview | Hopspot APK (`dioxus`) |
|---|---|---|
| Engine status / traffic | Static sample | `runtime_health` / snapshots |
| Interface list / detail / peers | Sample cards | `interface_snapshots` + `snapshots_to_cards` |
| Announce / Sleep / Power | Local toast | `engine::announce` / sleep / toggle |
| RNS Config | Sample template | Live `rpc_key` + ports |
| Limits | Sample rows | `GrowableHeap` limits |

## Not in scope for this demo

- Pixel clone of the OLED face (kept as `oled` flavor)
- LoRa tuner
- Replacing platform link Kotlin (USB/BLE/Wi‑Fi stay in `PrnsService`)
