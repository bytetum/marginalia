# Marginalia app

A Tauri 2 client for a Marginalia server, for Android, iOS, macOS, Windows and Linux. The interface is the server's own
web page (`../web`, shared and not copied: `frontendDist` points at it), running in a native shell that adds three
things a plain browser tab cannot do:

- the access token lives in the system credential store, not in page storage;
- "Share" > Marginalia on iOS and Android saves a link through `POST /articles`;
- article links open in the system browser.

The app talks to the server over the network, so the device must reach it: on Tailscale for the default
`https://marginalia-bytetum.taild98079.ts.net` (or any URL you type on the first-run screen, including a plain
`http://100.x.y.z:7878` tailnet address).

## First run

The setup screen asks for the server URL and the access token (`MARGINALIA_TOKEN` of the server). **Test connection**
calls `GET /health` (no token) and then an authenticated call, and says which step failed. **Connect** stores the
settings. Later, **Settings** (top right) changes the server or the token, and **Sign out** forgets the token.

## Where the token is kept

| Platform | Store |
| --- | --- |
| macOS | Keychain |
| iOS | Keychain (the app's own access group) |
| Windows | Credential Manager |
| Android | `SharedPreferences` encrypted with a key held in the Android Keystore (`android-native-keyring-store`) |
| Linux | Secret Service (GNOME Keyring, KWallet) |

All through `keyring-core` and its per-platform store crates (`src-tauri/src/secret.rs`). Android has a Keystore-backed
store, so no weaker app-private-file choice was needed there. If a platform has no working store (a Linux box without a
Secret Service, or an unexpected failure on a device) the token goes into a `0600` file in the app's private data
folder and Settings says so. The server URL is not a secret and lives in the webview's `localStorage`.

## Sharing a link to Marginalia

Both platforms hand the shared intent to Rust (`tauri-plugin-mobile-sharetarget` keeps it in a queue), and the page
pops the queue on start-up, when the app comes to the foreground, and on the `new-intent` event, so a cold start by the
share loses nothing. The page takes the first `http(s)` URL in the shared text, `POST`s it to `/articles`, and shows a
confirmation with an **Open** link (or the error). If the app is not set up yet the share stays queued until you finish
the setup screen.

- **Android:** `MainActivity` has an `ACTION_SEND` / `text/plain` intent filter (`gen/android/app/src/main/AndroidManifest.xml`).
  The plugin reads `Intent.EXTRA_TEXT` both at launch (`load`) and in `onNewIntent`.
- **iOS:** a Share Extension (`gen/apple/ShareExtension`, target `ShareExtension` in `gen/apple/project.yml`) accepts a
  web URL, a web page or text, pulls out the link and opens `marginalia://share?url=<encoded link>`. The app registers
  that scheme (`Info.ios.plist`, `tauri-plugin-deep-link`, registered before the share-target plugin), queues the URL and
  emits `new-intent`. Only `marginalia://share` is queued; anything else is ignored.
- **Desktop:** use the "Paste a URL to save" box on the Library screen (it is part of the shared page, so the browser
  page has it too).

## Security notes

- The webview only ever shows the app's own origin: `on_navigation` refuses everything else and new windows are denied.
  Links in articles are opened by the system browser (`tauri-plugin-opener`, `http` and `https` only).
- Capabilities (`src-tauri/capabilities`): event listen/unlisten, `opener:allow-open-url` for `https://*` and
  `http://*`, and on mobile `mobile-sharetarget:allow-pop-intent-queue`. The token commands are the app's own and need
  no plugin permission. There is no shell, fs or http plugin.
- CSP: `default-src 'none'`, scripts and styles from the app only, images from anywhere (articles link them).
  `connect-src` is `ipc: http://ipc.localhost https: http:`. It cannot name the server because the URL is typed at run
  time; script injection is what the CSP and the server's HTML sanitiser are there to stop, and the page loads no third
  party script.
- The server answers the app's origins with CORS (`tauri://localhost`, `http(s)://tauri.localhost`) and no others;
  authentication is the `Authorization: Bearer` header, never a cookie. See `src/api.rs`.
- Android allows cleartext HTTP (`usesCleartextTraffic`) so that `http://100.x.y.z:7878` works; the tailnet is
  encrypted by WireGuard anyway. Use the `https://` Tailscale Services URL when it is live. iOS exempts IP-address hosts
  from App Transport Security, so the same address works there.

## Installing the builds

CI attaches these to every release (`marginalia-app-<version>-<platform>`), each with a `.sha256` and a build-provenance
attestation (`gh attestation verify <file> --repo bytetum/marginalia`):

| File | Notes |
| --- | --- |
| `android-arm64.apk` | Signed with the release key. Allow "install unknown apps" for the browser or file manager, open the APK. Updates install over it because every release uses the same key and a growing `versionCode` (minutes since the epoch at the commit). Android 9 or newer. |
| `windows-x64-setup.exe` | NSIS installer, not signed: SmartScreen says "unknown publisher" (More info, Run anyway). |
| `macos-arm64.dmg` | Apple silicon, ad-hoc signed only (no Developer ID, not notarised). First start: right-click the app, Open; or `xattr -dr com.apple.quarantine /Applications/Marginalia.app`. |
| `linux-x86_64.AppImage`, `linux-amd64.deb` | `chmod +x` and run, or `sudo apt install ./….deb`. Needs webkit2gtk 4.1 (the deb depends on it). |
| `ios-unsigned.ipa` | Compiled without code signing, to prove the iOS target builds. Not installable as it is; see below. |

## Building

The Rust crate is its own workspace root, so `cargo test` at the repository root never touches it, and the Nix build
excludes `app/`. CI (`.github/workflows/release.yml`, jobs `app-*`) is the reference for every platform.

On NixOS, for the desktop build and tests:

```sh
cd app
nix-shell --run 'npm ci && (cd src-tauri && cargo test) && npx tauri build --no-bundle' </dev/null
./src-tauri/target/release/marginalia-app
```

`tauri.conf.json`, `Cargo.toml` and `package.json` must carry the server's version (CI fails otherwise); CI injects the
real release version with `--config '{"version": ...}'`. Icons come from `icon.svg` (`npx tauri icon icon.svg
--ios-color '#d97706'`).

## Android signing

`gen/android/app/build.gradle.kts` signs the release build when `ANDROID_KEYSTORE_PATH`, `ANDROID_KEYSTORE_PASSWORD`,
`ANDROID_KEY_ALIAS` and `ANDROID_KEY_PASSWORD` are set. CI takes them from the repository secrets
`ANDROID_KEYSTORE_BASE64` (the keystore, base64), `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_PASSWORD` and
`ANDROID_KEY_ALIAS`; without them (a fork's pull request) it builds an unsigned APK. The keystore is a PKCS12 file with
a 4096-bit RSA key valid for 100 years. The original lives in `~/.config/marginalia/android/` on the machine that made
it (`marginalia-release.keystore`, `store-password`, `key-alias`, all mode 600). **Keep a backup**: a release signed
with another key cannot be installed over the old one, you would have to uninstall first (the token is then asked
again).

## iOS

Everything iOS needs a Mac with Xcode; CI proves that the project compiles and archives without signing.

### Today, with a free Apple ID (7-day signing)

1. Install Xcode (App Store), start it once, accept the licence, `xcode-select --install`.
2. `brew install xcodegen node rustup` and `rustup-init` (or any rustup); the repository pins the Rust toolchain.
   `rustup target add aarch64-apple-ios`.
3. Clone the repository, then:
   ```sh
   cd app
   npm ci
   (cd src-tauri/gen/apple && xcodegen generate)
   ```
4. Plug in the iPhone, unlock it, trust the Mac, and switch on Settings > Privacy & Security > Developer Mode (the
   phone restarts).
5. Sign in to Xcode with the Apple ID (Settings > Accounts). Open `src-tauri/gen/apple/marginalia-app.xcodeproj`
   (launch Xcode from a terminal where `node` and `cargo` are on the PATH, e.g. `npx tauri ios build --open`, or the
   "Build Rust Code" phase cannot find them). Select **both** targets, `marginalia-app_iOS` and `ShareExtension`, and
   under Signing & Capabilities tick "Automatically manage signing" and choose your Personal Team. If Xcode says the
   bundle identifier is unavailable, change `com.bytetum.marginalia` and `com.bytetum.marginalia.ShareExtension` (keep
   the second one a child of the first) in `project.yml` and in `tauri.conf.json` (`identifier`), regenerate, and retry.
6. Choose the iPhone as the run destination and press Run. Or from the terminal, with the team id (Xcode > Settings >
   Accounts > team > ID): `APPLE_DEVELOPMENT_TEAM=<id> npx tauri ios dev` and pick the device.
7. On the phone: Settings > General > VPN & Device Management > your Apple ID > Trust.
8. In Safari or any app, Share > scroll the row of apps > More > enable **Marginalia** (first time only), then it is in
   the share sheet.

The profile expires after 7 days: connect the phone and press Run again to renew (the app and its data stay). A free
account allows three apps per device, and the Share Extension counts towards the App IDs per week. It does not need the
App Groups capability, which free accounts do not have, because the extension passes the link through a URL scheme and
not through shared storage.

### Later, with a paid Apple Developer account (TestFlight)

TODO, not done and not tested because it needs the account. In order:

1. Enrol in the Apple Developer Program. In Certificates, Identifiers & Profiles register two App IDs,
   `com.bytetum.marginalia` and `com.bytetum.marginalia.ShareExtension` (no capabilities needed). In App Store Connect
   create the app record for `com.bytetum.marginalia`.
2. Create an App Store Connect API key (Users and Access > Integrations; role App Manager) and note Issuer ID, Key ID and
   download the `.p8` once.
3. Add GitHub secrets: `APPLE_DEVELOPMENT_TEAM`, `APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_KEY_PRIVATE` (the `.p8`
   text). With the API key Xcode can create and fetch the distribution certificate and the two provisioning profiles
   itself (automatic signing with `-allowProvisioningUpdates`), which `tauri ios build` enables when `APPLE_API_*` are set.
   If you prefer manual signing, export a distribution certificate as `.p12` (secrets `APPLE_CERTIFICATE`,
   `APPLE_CERTIFICATE_PASSWORD`), import it into a temporary keychain in the job, and give `tauri ios build` the profile
   names through `APPLE_PROVISIONING_PROFILE` (and add the extension's profile in `project.yml`).
4. In the `app-ios` job replace `--no-sign` with `--export-method app-store-connect --build-number $GITHUB_RUN_NUMBER`,
   write the `.p8` to `$RUNNER_TEMP/AuthKey_<id>.p8` and export `API_PRIVATE_KEYS_DIR`, `APPLE_API_ISSUER`,
   `APPLE_API_KEY`, then upload the IPA with `xcrun altool --upload-app --type ios --file <ipa> --apiKey <id> --apiIssuer
   <issuer>` (only on `main` and tags, in `publish-app` or a job after it; never on pull requests).
5. Before the first upload make the Share Extension's `CFBundleShortVersionString` and `CFBundleVersion` follow the
   app's (App Store validation wants the same values; today both are `0.1.0` in `project.yml`, and `tauri ios build`
   only updates the app's). Also use a numeric `X.Y.Z` as the iOS version (no `-nightly.` suffix).
6. In App Store Connect > TestFlight add yourself as an internal tester and install through the TestFlight app. Builds
   last 90 days.

Until then the unsigned `.ipa` from CI can be signed with a free Apple ID by a re-signing tool such as Sideloadly or
AltStore, which also gives the 7-day profile; that is untested here.

## Not verified on real devices

CI builds every target and the Linux build was run end to end against a throwaway server (setup screen, library,
CORS, keyring and file fallback). Nothing has run on a real Android or iOS device, a Windows PC or a Mac: the share
target on both phones (the Android intent filter and JNI plugin, the iOS Share Extension and its deep link), the
Android Keystore and iOS Keychain stores, touch selection in the real WebViews, and the Windows and macOS installers.
