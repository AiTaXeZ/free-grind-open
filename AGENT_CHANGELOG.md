[2026-11-17 16:07] - Task: Implement Facebook OAuth token extraction and third-party login, update APK/IPA CI workflows
- Created: `AGENT_CHANGELOG.md`
- Modified: `src-tauri/src/api/rest.rs`
- Modified: `src-tauri/src/api/auth.rs`
- Modified: `src-tauri/src/lib.rs`
- Modified: `src-tauri/gen/android/app/src/main/AndroidManifest.xml`
- Modified: `src-tauri/gen/android/app/src/main/java/dev/estopia/free_grind/MainActivity.kt`
- Modified: `src-tauri/ios-keyboard-fix/ios/Sources/KeyboardFixPlugin.swift`
- Modified: `src-tauri/Info.plist`
- Modified: `src-tauri/gen/apple/project.yml`
- Modified: `src/types/api.ts`
- Modified: `src/contexts/auth-context.ts`
- Modified: `src/contexts/AuthContext.tsx`
- Modified: `src/pages/auth/SignInPage.tsx`
- Modified: `public/locales/en/translation.json`
- Modified: `.github/workflows/build-fork.yml`
- Modified: `.github/workflows/Build-iOS.yml`

[2026-11-17 16:21] - Task: Fix Build-iOS.yml Xcode project Rust bridge script patching
- Modified: `.github/workflows/Build-iOS.yml`

[2026-11-17 21:24] - Task: Normalize Grindr User-Agent version/build suffix, set HTTP/2 initial_stream_id(3), OkHttp pool/timeout settings, and en_US locale headers
- Modified: `src-tauri/src/api/headers.rs`

[2026-11-17 21:25] - Task: Align REST header ordering (Content-Type/Content-Length before Accept-Encoding) and add requirerealdeviceinfo/requireanondeviceinfo headers
- Modified: `src-tauri/src/api/rest.rs`

[2026-11-17 21:25] - Task: Replace custom FreeGrind WebView userAgent with standard Android Mobile Chrome UA
- Modified: `src-tauri/tauri.conf.json`

[2026-11-17 21:26] - Task: Add coarsenGeohash helper to match open-grind geohash precision
- Modified: `src/utils/geohash.ts`

[2026-11-17 21:27] - Task: Coarsen geohash, sync PUT /v4/location, support all cascade card variants, and batch-hydrate partial profiles via POST /v3/profiles
- Modified: `src/services/api/profileMethods.ts`

[2026-11-17 21:27] - Task: Add Free-Open push trigger to build-fork.yml and Build-iOS.yml workflows
- Modified: `.github/workflows/build-fork.yml`
- Modified: `.github/workflows/Build-iOS.yml`
