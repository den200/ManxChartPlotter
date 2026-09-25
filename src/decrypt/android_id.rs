//! The device identity o-charts' Android helper licenses charts to.
//!
//! On a desktop `oexserverd` reads the machine's own identity — MAC address,
//! hardware UUID, serial — into the fingerprint. The Android build reads
//! nothing of the sort: it writes into the fingerprint whatever ID the app
//! passes, and with none the identity is empty. It takes one of two:
//!
//! - `-y`, the device's Widevine ID: the DRM identifier Android keeps for the
//!   hardware, which survives reinstalls and resets. o-charts' own Android
//!   clients pass it where the device has Widevine.
//! - `-z`, anything else stable; here `ANDROID_ID`, for devices without
//!   Widevine such as the ODROID-C5 (ClearKey only). Since Android 8 it is
//!   scoped to the app's signing key, so the APK must always be signed with
//!   the same key or the device's licence is lost with it.

use std::sync::OnceLock;

/// Widevine's DRM scheme UUID, edef8ba9-79d6-4ace-a3c8-27dcd51d21ed.
const WIDEVINE_MSB: i64 = 0xedef8ba979d64ace_u64 as i64;
const WIDEVINE_LSB: i64 = 0xa3c827dcd51d21ed_u64 as i64;

/// Which identity the helper is given, and the flag it goes with.
pub enum DeviceId {
    Widevine(String),
    AndroidId(String),
}

impl DeviceId {
    pub fn flag(&self) -> &'static str {
        match self {
            DeviceId::Widevine(_) => "-y",
            DeviceId::AndroidId(_) => "-z",
        }
    }

    pub fn value(&self) -> &str {
        match self {
            DeviceId::Widevine(id) | DeviceId::AndroidId(id) => id,
        }
    }
}

/// This device's identity, read once: Widevine where there is one, else
/// ANDROID_ID. `None` leaves the device without an o-charts identity.
pub fn device_id() -> Option<&'static DeviceId> {
    static ID: OnceLock<Option<DeviceId>> = OnceLock::new();
    ID.get_or_init(|| {
        match java(read_widevine_id) {
            Ok(id) if !id.is_empty() => {
                log::info!("o-charts identity: Widevine device ID");
                return Some(DeviceId::Widevine(id));
            }
            Ok(_) => log::info!("Widevine returned an empty device ID"),
            // ClearKey-only devices throw UnsupportedSchemeException here.
            Err(e) => log::info!("no Widevine device ID ({e})"),
        }
        match java(read_android_id) {
            Ok(id) if !id.is_empty() => {
                log::info!("o-charts identity: ANDROID_ID");
                Some(DeviceId::AndroidId(id))
            }
            Ok(_) => {
                log::error!("ANDROID_ID is empty: no o-charts identity for this device");
                None
            }
            Err(e) => {
                log::error!("could not read ANDROID_ID: {e}");
                None
            }
        }
    })
    .as_ref()
}

/// Run `read` against the activity with its local references freed after,
/// and any Java exception cleared: a pending one fails every later JNI call.
fn java(
    read: fn(&mut jni::JNIEnv, &jni::objects::JObject) -> jni::errors::Result<String>,
) -> jni::errors::Result<String> {
    let context = ndk_context::android_context();
    // SAFETY: android-activity puts the process's JavaVM and the activity in
    // ndk-context before android_main runs.
    let vm = unsafe { jni::JavaVM::from_raw(context.vm().cast()) }?;
    let activity = unsafe { jni::objects::JObject::from_raw(context.context().cast()) };
    let mut env = vm.attach_current_thread()?;
    let result = env.with_local_frame(16, |env| read(env, &activity));
    if env.exception_check()? {
        env.exception_clear()?;
    }
    result
}

fn read_widevine_id(env: &mut jni::JNIEnv, _: &jni::objects::JObject) -> jni::errors::Result<String> {
    let uuid = env.new_object("java/util/UUID", "(JJ)V", &[WIDEVINE_MSB.into(), WIDEVINE_LSB.into()])?;
    let drm = env.new_object("android/media/MediaDrm", "(Ljava/util/UUID;)V", &[(&uuid).into()])?;
    let name = env.new_string("deviceUniqueId")?;
    let bytes = env
        .call_method(&drm, "getPropertyByteArray", "(Ljava/lang/String;)[B", &[(&name).into()])?
        .l()?;
    let bytes = env.convert_byte_array(jni::objects::JByteArray::from(bytes))?;
    env.call_method(&drm, "close", "()V", &[])?;
    Ok(bytes.iter().map(|b| format!("{b:02X}")).collect())
}

fn read_android_id(env: &mut jni::JNIEnv, activity: &jni::objects::JObject) -> jni::errors::Result<String> {
    let resolver = env
        .call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
        .l()?;
    let name = env.new_string("android_id")?;
    let id = env
        .call_static_method(
            "android/provider/Settings$Secure",
            "getString",
            "(Landroid/content/ContentResolver;Ljava/lang/String;)Ljava/lang/String;",
            &[(&resolver).into(), (&name).into()],
        )?
        .l()?;
    if id.is_null() {
        return Ok(String::new());
    }
    Ok(env.get_string(&jni::objects::JString::from(id))?.into())
}
