//! The alarm sound.
//!
//! No audio crate: the plotter needs exactly one sound, so it is made here
//! and handed to what every platform already has. On Linux (the Pi) and
//! macOS a two-tone WAV is written once to the temp directory and played by
//! the system's own player — `aplay`, else `paplay` / `pw-play`, or `afplay`.
//! On Android, `ToneGenerator` on the alarm stream, which sounds even with
//! the media volume down. A missing player or sound device is logged once
//! and otherwise ignored: the alarm is still on screen.

/// Sound the alarm once (about half a second). Never blocks, and does
/// nothing while the previous sound is still playing.
pub fn alarm() {
    #[cfg(target_os = "android")]
    android::tone();
    #[cfg(not(target_os = "android"))]
    desktop::play();
}

/// Two tones, high then low — the "hi-lo" every marine alarm uses, because
/// it cuts through engine and wind noise better than one pitch.
fn wav() -> Vec<u8> {
    const RATE: u32 = 22_050;
    let tone = |freq: f32, secs: f32, out: &mut Vec<i16>| {
        let n = (RATE as f32 * secs) as usize;
        let fade = (RATE as f32 * 0.01) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            // Short fades at each end: a tone that starts at full amplitude
            // clicks.
            let env = (i.min(n - 1 - i) as f32 / fade as f32).min(1.0);
            let s = (t * freq * std::f32::consts::TAU).sin() * env * 0.6;
            out.push((s * i16::MAX as f32) as i16);
        }
    };
    let mut samples = Vec::new();
    tone(1_000.0, 0.22, &mut samples);
    tone(700.0, 0.22, &mut samples);
    let data_len = (samples.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes()); // PCM header size
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&RATE.to_le_bytes());
    b.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate
    b.extend_from_slice(&2u16.to_le_bytes()); // block align
    b.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&s.to_le_bytes());
    }
    b
}

#[cfg(not(target_os = "android"))]
mod desktop {
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;

    static PLAYING: Mutex<Option<Child>> = Mutex::new(None);

    fn file() -> Option<PathBuf> {
        static FILE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        FILE.get_or_init(|| {
            let path = std::env::temp_dir().join("manx-alarm.wav");
            match std::fs::write(&path, super::wav()) {
                Ok(()) => Some(path),
                Err(e) => {
                    log::warn!("alarm sound: cannot write {}: {e}", path.display());
                    None
                }
            }
        })
        .clone()
    }

    fn players() -> &'static [&'static [&'static str]] {
        if cfg!(target_os = "macos") {
            &[&["afplay"]]
        } else {
            &[&["aplay", "-q"], &["paplay"], &["pw-play"]]
        }
    }

    pub fn play() {
        let Some(path) = file() else { return };
        let mut playing = PLAYING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = playing.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                return; // still sounding
            }
        }
        for cmd in players() {
            let spawned = Command::new(cmd[0])
                .args(&cmd[1..])
                .arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(child) = spawned {
                *playing = Some(child);
                return;
            }
        }
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| log::warn!("alarm sound: no audio player found ({:?})", players()));
    }
}

#[cfg(target_os = "android")]
mod android {
    use std::sync::OnceLock;

    /// `ToneGenerator.TONE_CDMA_ALERT_CALL_GUARD`: a two-tone alert.
    const TONE: i32 = 93;
    /// `AudioManager.STREAM_ALARM`.
    const STREAM_ALARM: i32 = 4;

    static GENERATOR: OnceLock<Option<jni::objects::GlobalRef>> = OnceLock::new();

    pub fn tone() {
        let result = (|| -> jni::errors::Result<()> {
            let context = ndk_context::android_context();
            // SAFETY: android-activity puts the process's JavaVM in
            // ndk-context before android_main runs.
            let vm = unsafe { jni::JavaVM::from_raw(context.vm().cast()) }?;
            let mut env = vm.attach_current_thread()?;
            let generator = GENERATOR.get_or_init(|| {
                let made = env.with_local_frame(4, |env| {
                    let g = env.new_object(
                        "android/media/ToneGenerator",
                        "(II)V",
                        &[STREAM_ALARM.into(), 100.into()],
                    )?;
                    env.new_global_ref(g)
                });
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_clear();
                }
                made.map_err(|e| log::warn!("alarm sound: no ToneGenerator: {e}")).ok()
            });
            let Some(generator) = generator else { return Ok(()) };
            env.call_method(generator, "startTone", "(II)Z", &[TONE.into(), 500.into()])?;
            if env.exception_check()? {
                env.exception_clear()?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            log::warn!("alarm sound: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_alarm_is_a_well_formed_wav() {
        let w = super::wav();
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        let data_len = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
        assert_eq!(w.len(), 44 + data_len);
        assert!(data_len > 10_000, "about half a second of sound");
    }
}
