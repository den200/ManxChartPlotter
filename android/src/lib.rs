//! navcore on Android: `android_main` is what the NativeActivity calls.
//! The app itself is src/bin/navcore.rs, included whole; its desktop-only
//! parts (the command-line modes, `main`) are simply never called here.

#[path = "../../src/bin/navcore.rs"]
#[allow(dead_code, unused_imports)]
mod app;

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(android: winit::platform::android::activity::AndroidApp) {
    app::android_start(android);
}
