//! Handing a file to the sailor: a logbook day, a route.
//!
//! On a desktop or the Pi it is written to the Downloads folder. On Android
//! an app cannot write there directly, and a file in its own storage is out
//! of reach — so the file goes into Download/navcore through MediaStore
//! (Android 10+, no permission needed), and the share sheet opens with it,
//! for mail, Drive, a messenger or another plotter. Cancelling the sheet
//! leaves the file in Downloads. On Android 8–9, which have no MediaStore
//! downloads, the share sheet carries the content as text.
//!
//! The APK has no Java code, so all of it is JNI calls on the platform's own
//! classes, like the soft keyboard and the alarm tone.

/// Save `bytes` as `name` and offer it onward. Returns where it went, for
/// the status line.
pub fn save(name: &str, mime: &str, bytes: &[u8]) -> Result<String, String> {
    #[cfg(target_os = "android")]
    {
        android::save_and_share(name, mime, bytes)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = mime;
        let dir = crate::nav::logbook::export_dir();
        let path = dir.join(name);
        std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(&path, bytes))
            .map(|_| format!("Saved {}", path.display()))
            .map_err(|e| format!("Could not save {}: {e}", path.display()))
    }
}

/// A file name from anything a user typed: letters, digits and dashes.
pub fn file_name(stem: &str, ext: &str) -> String {
    let clean: String = stem
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    format!("{clean}.{ext}")
}

#[cfg(target_os = "android")]
mod android {
    use jni::objects::{JObject, JValue};
    use jni::JNIEnv;

    /// `Intent.FLAG_GRANT_READ_URI_PERMISSION`
    const GRANT_READ: i32 = 1;

    pub fn save_and_share(name: &str, mime: &str, bytes: &[u8]) -> Result<String, String> {
        let context = ndk_context::android_context();
        // SAFETY: android-activity puts the JavaVM and the activity in
        // ndk-context before android_main runs.
        let vm = unsafe { jni::JavaVM::from_raw(context.vm().cast()) }.map_err(|e| e.to_string())?;
        let activity = unsafe { JObject::from_raw(context.context().cast()) };
        let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
        let result = env.with_local_frame(32, |env| -> jni::errors::Result<String> {
            let sdk = env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?.i()?;
            let intent = env.new_object("android/content/Intent", "()V", &[])?;
            let action = env.new_string("android.intent.action.SEND")?;
            env.call_method(&intent, "setAction", "(Ljava/lang/String;)Landroid/content/Intent;", &[(&action).into()])?;
            let where_ = if sdk >= 29 {
                let uri = save_to_downloads(env, &activity, name, mime, bytes)?;
                let jmime = env.new_string(mime)?;
                env.call_method(&intent, "setType", "(Ljava/lang/String;)Landroid/content/Intent;", &[(&jmime).into()])?;
                let key = env.new_string("android.intent.extra.STREAM")?;
                env.call_method(
                    &intent,
                    "putExtra",
                    "(Ljava/lang/String;Landroid/os/Parcelable;)Landroid/content/Intent;",
                    &[(&key).into(), (&uri).into()],
                )?;
                env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[GRANT_READ.into()])?;
                format!("Saved Download/navcore/{name}")
            } else {
                let text = env.new_string(String::from_utf8_lossy(bytes))?;
                let plain = env.new_string("text/plain")?;
                env.call_method(&intent, "setType", "(Ljava/lang/String;)Landroid/content/Intent;", &[(&plain).into()])?;
                let key = env.new_string("android.intent.extra.TEXT")?;
                env.call_method(
                    &intent,
                    "putExtra",
                    "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
                    &[(&key).into(), (&text).into()],
                )?;
                let subject_key = env.new_string("android.intent.extra.SUBJECT")?;
                let subject = env.new_string(name)?;
                env.call_method(
                    &intent,
                    "putExtra",
                    "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
                    &[(&subject_key).into(), (&subject).into()],
                )?;
                format!("Shared {name}")
            };
            let title = env.new_string(format!("Share {name}"))?;
            let chooser = env
                .call_static_method(
                    "android/content/Intent",
                    "createChooser",
                    "(Landroid/content/Intent;Ljava/lang/CharSequence;)Landroid/content/Intent;",
                    &[(&intent).into(), (&title).into()],
                )?
                .l()?;
            env.call_method(&activity, "startActivity", "(Landroid/content/Intent;)V", &[(&chooser).into()])?;
            Ok(where_)
        });
        // A Java exception left pending would fail every later JNI call; and
        // it is the real reason, where there is one.
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        result.map_err(|e| format!("Could not export {name}: {e}"))
    }

    /// Insert the file into MediaStore's Downloads, under navcore/, and
    /// write it. Returns its content:// URI.
    fn save_to_downloads<'a>(
        env: &mut JNIEnv<'a>,
        activity: &JObject,
        name: &str,
        mime: &str,
        bytes: &[u8],
    ) -> jni::errors::Result<JObject<'a>> {
        let resolver = env
            .call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
            .l()?;
        let values = env.new_object("android/content/ContentValues", "()V", &[])?;
        for (k, v) in [("_display_name", name), ("mime_type", mime), ("relative_path", "Download/navcore")] {
            let (k, v) = (env.new_string(k)?, env.new_string(v)?);
            env.call_method(&values, "put", "(Ljava/lang/String;Ljava/lang/String;)V", &[(&k).into(), (&v).into()])?;
        }
        let collection = env
            .get_static_field("android/provider/MediaStore$Downloads", "EXTERNAL_CONTENT_URI", "Landroid/net/Uri;")?
            .l()?;
        let uri = env
            .call_method(
                &resolver,
                "insert",
                "(Landroid/net/Uri;Landroid/content/ContentValues;)Landroid/net/Uri;",
                &[(&collection).into(), (&values).into()],
            )?
            .l()?;
        if uri.is_null() {
            return Err(jni::errors::Error::NullPtr("MediaStore refused the file"));
        }
        let stream = env
            .call_method(&resolver, "openOutputStream", "(Landroid/net/Uri;)Ljava/io/OutputStream;", &[(&uri).into()])?
            .l()?;
        let array = env.byte_array_from_slice(bytes)?;
        env.call_method(&stream, "write", "([B)V", &[JValue::Object(&array)])?;
        env.call_method(&stream, "close", "()V", &[])?;
        Ok(uri)
    }
}
