# Releasing Recap (macOS)

The ordered procedure for turning this tree into a `Recap.app` that a stranger
can download, open, and use. Run the steps in order — several of them invalidate
the ones before (signing seals the bundle contents, so anything that touches
`Contents/` after step 4 breaks the signature).

Set these once per shell; every command below uses them.

```sh
cd /path/to/recap
export APP="src-tauri/target/release/bundle/macos/Recap.app"
export ENT="src-tauri/entitlements.plist"
export IDENT="Developer ID Application: Your Name (TEAMID)"   # see step 1
```

---

## 0. Prerequisites

| Need | Check |
|---|---|
| Xcode Command Line Tools | `xcode-select -p` |
| Rust + Tauri CLI v2 | `cargo tauri --version` |
| Apple Developer Program membership | required — a free account cannot notarize |
| A **Developer ID Application** certificate + private key in the login keychain | `security find-identity -v -p codesigning` |
| `vendor/ffmpeg` | `file vendor/ffmpeg` |

### `vendor/ffmpeg` is a build input now, not a convenience

`bundle.resources` in `tauri.conf.json` copies `vendor/ffmpeg` into
`Recap.app/Contents/Resources/ffmpeg`, and that copy is the *only* ffmpeg an
installed app can find — `ffmpeg::locate()`'s dev-tree walk has no source tree to
walk once the app is in `/Applications`. `vendor/` is git-ignored, so a fresh
clone has none and `cargo tauri build` will fail at the bundling step. That
failure is deliberate: shipping an app with no capture engine is worse.

Put a static build with `avfoundation` and `videotoolbox` there — e.g. the macOS
binaries from <https://evermeet.cx/ffmpeg/> or <https://osxexperts.net>. Confirm
the two capture inputs are actually compiled in:

```sh
vendor/ffmpeg -hide_banner -devices  2>&1 | grep avfoundation
vendor/ffmpeg -hide_banner -encoders 2>&1 | grep videotoolbox
```

**Architecture decides who can run the release.** A thin arm64 ffmpeg makes the
whole app Apple-Silicon-only, however the Rust side was built, because ffmpeg is
the capture engine. Check, and build a universal one if Intel matters:

```sh
lipo -info vendor/ffmpeg              # "Non-fat file: ... is architecture: arm64"
lipo -create ffmpeg-arm64 ffmpeg-x86_64 -output vendor/ffmpeg
```

A universal *app* then also needs `rustup target add x86_64-apple-darwin` and
`cargo tauri build --target universal-apple-darwin` in step 3 (the bundle path
gains a `universal-apple-darwin/` component).

---

## 1. Identify the signing certificate

```sh
security find-identity -v -p codesigning
```

Copy the quoted name of the **Developer ID Application** line into `$IDENT`.
Ignore any "Apple Development" / "Mac Developer" identity — those are for local
runs and cannot be notarized.

`0 valid identities found` means there is no certificate here: create one at
<https://developer.apple.com/account/resources/certificates> (Developer ID
Application), download the `.cer`, and double-click it to import. You cannot
sign or notarize without it, and an unsigned or ad-hoc-signed `Recap.app` is
blocked by Gatekeeper on any machine that didn't build it.

---

## 2. Version bump

Two files, kept identical — Tauri reads `tauri.conf.json` and falls back to
`Cargo.toml`, so a mismatch produces an installer whose version disagrees with
the binary's.

```sh
# src-tauri/tauri.conf.json  ->  "version": "0.2.0"
# src-tauri/Cargo.toml       ->  version = "0.2.0"
cargo check --manifest-path src-tauri/Cargo.toml   # refreshes Cargo.lock
git diff src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock
```

Then run the tests, because everything after this point is expensive:

```sh
cargo test --manifest-path src-tauri/Cargo.toml
```

---

## 3. Build the bundle

```sh
cargo tauri build --bundles app
```

Several minutes: the release profile is `lto = true`, `codegen-units = 1`.
Produces `$APP`.

Every build prints `The bundle identifier "dev.recap.app" ... ends with '.app'`.
It is cosmetic and has been left alone on purpose: the identifier is the key
macOS uses for the Screen Recording TCC grant and for the settings directory, so
changing it silently revokes every existing user's permission and orphans their
config. If it is ever changed, that is a release note, not a cleanup.

Two things this build gets right for free, worth knowing rather than
rediscovering:

- `devctl` — the loopback JSON socket with arbitrary JS `eval` — is
  `#[cfg(debug_assertions)]`, so it is compiled out here. Verified in step 6.
- Tauri merges `src-tauri/Info.plist` into the bundle, which is where
  `NSMicrophoneUsageDescription` comes from. Without it macOS terminates the
  process the moment ffmpeg opens the mic.

Confirm ffmpeg made it in before spending a notarization round-trip on it:

```sh
ls -l "$APP/Contents/Resources/ffmpeg"
"$APP/Contents/Resources/ffmpeg" -version | head -1
```

---

## 4. Sign — nested code first, outer bundle last

Order is not stylistic. The app's signature seals `Contents/`, so signing the
outer bundle first and the nested ffmpeg second leaves the outer signature
referring to a file that no longer matches, and `codesign --verify` fails.

Both binaries get the same entitlements file, for the reason documented at the
top of `src-tauri/entitlements.plist`: ffmpeg is the process that opens the
microphone, and the entitlement check is per-signature.

```sh
codesign --force --timestamp --options runtime \
  --entitlements "$ENT" --sign "$IDENT" \
  "$APP/Contents/Resources/ffmpeg"

codesign --force --timestamp --options runtime \
  --entitlements "$ENT" --sign "$IDENT" \
  "$APP"
```

`--options runtime` enables the hardened runtime, which notarization requires.
`--timestamp` fetches a secure timestamp from Apple, also required — it is what
keeps the signature valid after the certificate expires, and it needs network
access.

<details>
<summary>The <code>--deep</code> one-liner, and why it is not the default here</summary>

```sh
codesign --force --deep --timestamp --options runtime \
  --entitlements "$ENT" --sign "$IDENT" "$APP"
```

This does work for a bundle as simple as Recap's — one nested Mach-O, and the
entitlements we want applied to both. But `--deep` is documented by Apple as
unsuitable for distribution signing: it applies the *outer* entitlements to
every nested binary it discovers, whatever they are, and it silently signs
things you did not know were in the bundle. Explicit inside-out signing is
what should run in a release script; keep `--deep` for one-off local checks.

</details>

### Alternative: let Tauri sign during the build

Tauri's bundler signs and notarizes itself when these are set, which is the
right shape for CI:

```sh
export APPLE_SIGNING_IDENTITY="$IDENT"
export APPLE_ID="you@example.com"
export APPLE_PASSWORD="abcd-efgh-ijkl-mnop"    # app-specific password
export APPLE_TEAM_ID="TEAMID"
cargo tauri build --bundles app,dmg
```

It reads `bundle.macOS.entitlements` from `tauri.conf.json`, so the same
`entitlements.plist` applies. Steps 5–7 then happen inside the build; still run
step 6's verification afterwards.

---

## 5. Verify the signature locally

Cheap, offline, and catches almost everything notarization would reject.

```sh
# Structural validity, including nested code.
codesign --verify --deep --strict --verbose=2 "$APP"

# Hardened runtime actually on: the flags line must contain `runtime`.
codesign -dv "$APP" 2>&1 | grep -E 'flags|Authority|TeamIdentifier|Timestamp'

# Entitlements that really got baked in — expect exactly audio-input, on both.
# Use `--entitlements -`, not the `:-` form seen in older writeups; codesign
# now warns that the colon is deprecated.
codesign -d --entitlements - --xml "$APP" | plutil -p -
codesign -d --entitlements - --xml "$APP/Contents/Resources/ffmpeg" | plutil -p -

# Must print nothing. get-task-allow is a hard notarization rejection.
codesign -d --entitlements - --xml "$APP" | grep get-task-allow
```

At this point `spctl -a -vvv -t exec "$APP"` still says **rejected** — the app
is signed but not yet notarized. That is expected; step 7 is what changes it.

---

## 6. Release-build sanity checks

Things that are only true of a release build, and are cheap to confirm.

The executable inside the bundle is lowercase `recap` — `productName` names the
`.app`, the Cargo package names the binary. `$APP/Contents/MacOS/Recap` does not
exist.

```sh
# devctl — the loopback socket with arbitrary JS eval — must be compiled out.
# `nm` is useless here (profile.release sets strip = true, so it finds nothing
# either way); the log line it prints on startup is the honest marker.
strings -a "$APP/Contents/MacOS/recap" | grep -c "devctl: listening"   # 0
strings -a src-tauri/target/debug/recap | grep -c "devctl: listening"  # 1

# ffmpeg is present, executable, and its own signature is intact.
codesign --verify --strict "$APP/Contents/Resources/ffmpeg"

# NSMicrophoneUsageDescription actually merged in from src-tauri/Info.plist.
plutil -extract NSMicrophoneUsageDescription raw "$APP/Contents/Info.plist"
```

### The one check that matters: does the shipped app find its own ffmpeg

Reproduce a machine that has never seen this repo. Copy the bundle somewhere
with no `vendor/` directory anywhere above it — otherwise `locate()`'s dev-tree
walk finds the repo's copy and the test proves nothing — and strip the
environment so neither `RECAP_FFMPEG` nor a PATH ffmpeg can stand in.

`--region` is load-bearing: it forces the crop pass, which is the code path that
shells out to ffmpeg. A plain `shot` only runs Apple's `screencapture` and would
pass even with no ffmpeg in the bundle at all.

```sh
rm -rf /tmp/virgin && mkdir -p /tmp/virgin
ditto "$APP" /tmp/virgin/Recap.app

env -i PATH=/usr/bin:/bin HOME="$HOME" \
  /tmp/virgin/Recap.app/Contents/MacOS/recap \
  shot --region 0,0,320,240 /tmp/virgin/smoke.png

sips -g pixelWidth -g pixelHeight /tmp/virgin/smoke.png   # must be 320 x 240
```

Exit 0 and a 320×240 PNG means the bundled ffmpeg resolved. A
`ffmpeg not found.` error means `locate()` did not find
`Contents/Resources/ffmpeg` and the release is broken — that is precisely the
bug this bundling exists to fix.

Worth running the negative control once when touching `ffmpeg::locate()`, since
a false pass here ships a dead app:

```sh
mv /tmp/virgin/Recap.app/Contents/Resources/ffmpeg /tmp/ffmpeg.hidden
env -i PATH=/usr/bin:/bin HOME="$HOME" \
  /tmp/virgin/Recap.app/Contents/MacOS/recap \
  shot --region 0,0,320,240 /tmp/virgin/smoke2.png   # must fail: ffmpeg not found
mv /tmp/ffmpeg.hidden /tmp/virgin/Recap.app/Contents/Resources/ffmpeg
```

(`shot` needs Screen Recording permission for the binary being run. If it fails
with the "macOS is almost certainly blocking screen recording" hint instead of
the ffmpeg one, that is TCC, not ffmpeg.)

---

## 7. Notarize

`notarytool` will not take a bare `.app` directory — submit a zip or a dmg. Use
`ditto`, never `zip`: `zip` mangles symlinks and extended attributes and the
signature does not survive.

Store the credentials in the keychain once, so the password stops appearing in
shell history and CI logs:

```sh
xcrun notarytool store-credentials "recap-notary" \
  --apple-id "you@example.com" \
  --team-id "TEAMID" \
  --password "abcd-efgh-ijkl-mnop"    # app-specific pw from appleid.apple.com
```

Then, per release:

```sh
mkdir -p dist
ditto -c -k --keepParent "$APP" dist/Recap.zip

xcrun notarytool submit dist/Recap.zip \
  --keychain-profile "recap-notary" --wait
```

`--wait` blocks until Apple returns `Accepted` or `Invalid` — usually a few
minutes. On `Invalid`, the summary tells you nothing useful; the log does:

```sh
xcrun notarytool log <submission-id> --keychain-profile "recap-notary"
xcrun notarytool history --keychain-profile "recap-notary"
```

The two failures to expect here are `The signature does not include a secure
timestamp` (missing `--timestamp`) and `The executable does not have the
hardened runtime enabled` naming `Contents/Resources/ffmpeg` (step 4 skipped the
nested binary).

---

## 8. Staple

Notarization registers the app with Apple; stapling attaches the ticket to the
bundle so Gatekeeper can approve it **offline**. Skipping this leaves first
launch dependent on the user's network.

The ticket is stapled to the `.app`, not to the zip you submitted — then you
re-package. Repackaging after stapling is safe: it adds a file to
`Contents/CodeResources`' notarization slot, not to the signed payload.

```sh
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"          # "The validate action worked!"
```

---

## 9. Final verification, as Gatekeeper sees it

```sh
spctl -a -vvv -t exec "$APP"
# expect: accepted / source=Notarized Developer ID
```

Then package for distribution and take the dmg through the same
notarize + staple loop, because the disk image is a separate artifact with its
own ticket:

```sh
hdiutil create -volname "Recap" -srcfolder "$APP" -ov -format UDZO dist/Recap.dmg
codesign --force --timestamp --sign "$IDENT" dist/Recap.dmg
xcrun notarytool submit dist/Recap.dmg --keychain-profile "recap-notary" --wait
xcrun stapler staple dist/Recap.dmg
spctl -a -vvv -t open --context context:primary-signature dist/Recap.dmg
```

---

## 10. Smoke-test as a real user

The point of the whole exercise is a machine that has never seen this repo, so
test on one — or at minimum simulate the quarantine flag that makes Gatekeeper
actually run its checks. Copying a file locally never sets it; downloading does.

```sh
xattr -w com.apple.quarantine "0083;00000000;Recap;" dist/Recap.dmg
open dist/Recap.dmg          # no "unidentified developer" / "damaged" dialog
```

On the test machine, in order:

1. Drag Recap to `/Applications` and launch it.
2. Grant **System Settings → Privacy & Security → Screen Recording**, then
   restart the app. macOS keys this on the signed bundle identity, so with a
   stable Developer ID it is granted once and survives updates — unlike
   `cargo tauri dev`, where the binary's identity changes every rebuild.
3. Take a still. The annotation editor should open.
4. Record with mic audio on — this is the microphone prompt and the one thing
   that fails loudly if `entitlements.plist` did not reach `Resources/ffmpeg`.
5. Pause and resume, then stop: exercises the segment concat, which shells out
   to the bundled ffmpeg a second time.
6. `Ctrl+Alt+T` text grab — Vision.framework, no ffmpeg involved.

---

## Rollback

Nothing here mutates anything shared: a bad release is a bad artifact in
`dist/`, so delete it and re-cut. Notarization tickets cannot be withdrawn, but
an un-stapled, un-distributed submission is inert. If a signed build has already
shipped and must be killed, that is certificate revocation at
<https://developer.apple.com/account/resources/certificates> — which invalidates
**every** build signed with that certificate, not just the bad one. Prefer
shipping a fixed version.
