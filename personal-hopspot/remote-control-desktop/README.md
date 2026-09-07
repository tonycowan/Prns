# Personal Hopspot Remote Control Desktop

A desktop-first Dioxus client for operating paired Personal Hopspot
remote-control targets. The window is titled **PRNS Controller**. Use the
nodes icon (left) and settings icon (right) in the top bar to switch
between **Managed Nodes** (the default) and **Settings**.

## Prerequisites

Install the Dioxus CLI at the same version as the application:

```console
cargo install dioxus-cli --version 0.7.5 --locked
```

## Run

From this directory, use either:

```console
dx serve --desktop
```

or:

```console
cargo run --features desktop
```

## Android (arm64)

The same crate builds as **PRNS Controller** for Android via Dioxus mobile.
BLE Auto, USB Auto, TCP, and Auto Wi-Fi attach there the same way they do
on desktop. BLE starts on; USB, Auto Wi-Fi, and TCP start off. Grant
Bluetooth permissions when the app asks. USB uses the Hopspot JNI host
(WebUSB `1209:0001` and Android accessory). Pair over BLE, USB, LAN, or a
TCP path to `prnsd`. Do not run this app's BLE and Hopspot's BLE on the
same phone at once.

```console
export JAVA_HOME="/Library/Java/JavaVirtualMachines/temurin-17.jdk/Contents/Home"
export ANDROID_HOME="/opt/homebrew/share/android-commandlinetools"
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/27.2.12479018"
export NDK_HOME="$ANDROID_NDK_HOME"
dx build --android --release --target aarch64-linux-android \
  --no-default-features --features mobile
adb install -r target/dx/personal-hopspot-remote-control-desktop/release/android/app/app/build/outputs/apk/debug/app-debug.apk
```

Application id: `org.personal.prns.controller`. Identities persist under
the app files directory (`…/files/hopspot-remote-control`).

## Configure the controller

The application always starts its own Tokio `personal-rns` controller node.
It does not attach to a running `prnsd` shared instance. Settings shows this
node's local interfaces; target sections show the remote device's
interfaces. The two communicate only over a shared transport (Auto Wi-Fi, TCP, BLE,
or USB), as two separate nodes:

```console
HOPSPOT_RC_TCP=127.0.0.1:4242 cargo run --features desktop
```

Only BLE Auto starts on. Set `HOPSPOT_RC_BLE=0` to boot it off; Settings
Start/Stop turns each local transport up or down without restarting. USB,
Auto Wi-Fi, and TCP start off. Set `HOPSPOT_RC_USB=1` or
`HOPSPOT_RC_AUTO_WIFI=1` to start those on. A TCP client is always listed,
default off, aiming at `127.0.0.1:4242`. `HOPSPOT_RC_TCP=host:port` chooses
another target and starts that client on. Do not run this app's BLE and a local `prnsd` Bluetooth Auto at
the same time. Pairing ads stay hop-0; `prnsd` will not forward them.
Stable Operator and Instance identities, the host BLE identity, and paired-target
access are stored under `~/.local/share/hopspot-remote-control`. Override
that location with `HOPSPOT_RC_DATA_DIR`. Settings shows the Operator hash
   that pairing writes onto the target (`identities/controller`) and this
   install's Instance hash (`identities/instance`). Sibling Controllers
   share Operator only. The BLE identity is `identities/ble` in that
   directory. Peer aliases live in `peer-aliases` there, keyed by the
   full interface id (the `P XXXX` label is a durable short prefix of that id).
   Target pairing names live in `target-names` (the announcement label from
   pairing). Optional target aliases live in `target-aliases` beside that
   name. Pairing does not fill the alias; type one if you want a second
   label. Sibling aliases live in `sibling-aliases`, keyed by Instance hash.
   Adopt can set the name; otherwise the app stores `Sibling 1`, `Sibling 2`,
   and so on. Settings → Sibling Controllers lists adopted installs by that
   alias. Configure slides in USB adopt: dest arms **Allow a sibling to
   replace me**, source presses **Share Operator**, both confirm the six
   digits. Dest's Operator is replaced; Instance stays. Quit and reopen the
   dest app after adopt. Sibling roster sync uses Instance after that and
   copies pairing names, target aliases, manager aliases, sibling aliases
   for other Instances, and non-USB peer aliases when both apps have heard
   each other. This install's own sibling-alias row is local-only.

## Pair a target

Pairing availability is broadcast once when the target opens its window. Start
the controller first so it is already listening. Heard announcements show up on
**Managed Nodes** as “Awaiting pairing”.

1. Start the desktop controller on a transport shared with the target
   (BLE Auto is on by default; enable USB, Auto Wi-Fi, or TCP on Settings, or set
   `HOPSPOT_RC_USB=1` / `HOPSPOT_RC_AUTO_WIFI=1` / `HOPSPOT_RC_TCP=…`). For MeshTower /
   T1000-E, wait for a BLE peer on Settings, then Pair remote.
2. Then open pairing on the target: `prnsd pairing open`, or **Pair remote** on
   a Hopspot face. Note the invitation code.
3. On **Managed Nodes**, expand the awaiting entry and enter that code, then
   **Continue**.
4. Compare the six confirmation digits on both devices (`prnsd pairing status`
   prints them). Approve on either side, in either order. The desktop
   waits if the target has not finished yet.
5. Expand the new paired target to see its interfaces. The firmware / PRNS
   version appears next to the name after the controller first reaches that
   node. The row keeps the pairing announcement name; type an optional alias
   beside it. The list uses any
   heard route to the target's remote-control destination (the pairing
   advertisement does not install that route). If this app Approves
   first, that first inventory can miss until the target Approves; the
   app retries for about two minutes. The interface list is the
   same configured set `prnsd status -a` shows, not each discovered peer.
   The list, each interface's config, and each fleet's peers are separate
   requests so one packet never has to carry every fact.
   After the interface list, **Node Management Whitelist** lists each
   manager address hash, an optional alias, and Remove. A narrow window
   truncates those text fields; click a hash to read it in full. Configure
   adds a controller with the 128-character allow-list key from Settings.
   Expand an interface for the same card and menu facts Hopspot shows
   (mode, connection, group, IFAC, traffic, rates, links, destinations,
   activity, role/host, drops, per-peer traffic/link/rate/activity, and
   the radio sample that medium owns)
   plus **Start** / **Stop** and **Configure**. Each peer shows a durable
   `P XXXX` id and an optional alias you can type beside it. LAN / Auto Wi-Fi and
   Bluetooth Auto cards also show the discovery group id. Settings
   Start/Stop and Configure match a target card. Configure slides the status card
   aside and slides **Start** / **Stop** and **Configure** off so **Back** and
   **Save** take that same left-aligned row. Mode, group, and LoRa tune
   edits stay local until    **Save**. A Hopspot Auto Wi-Fi card can set the station SSID and
   password (empty password is an open network; saving replaces the stored
   password). A LoRa or RNode card can change region,
   frequency, preset, SF, bandwidth, CR, TX power, and preamble. **Back**
   returns to the status card when nothing has changed; **Cancel**
   does the same after edits and warns if anything is still unsaved.
   Settings and the target list
   refresh about once a second;
   ↻ on a target, interface, or peer reloads that host's inventory.
   Below the target
   address are
   two lines: last announce time in local time, then hop count and inbound path.
   Stored targets come back Offline after a restart; the app probes each
   one with a path request, and **Find path** retries that. **Forget**
   drops the stored pairing from this controller.

If you opened pairing before starting the app, open it again (or close and
re-open) so a fresh advertisement is heard. Configuration changes made
through this app are listed under Settings → Activity log.
