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

    /// The player now sounding, and which of [`players`] it is.
    static PLAYING: Mutex<Option<(Child, usize)>> = Mutex::new(None);
    /// The first player to try. Moves on when one starts but fails — an
    /// `aplay` with no ALSA device to play to exits at once, and trying it
    /// again every three seconds would keep the alarm silent for good with
    /// `paplay` sitting unused beside it.
    static FIRST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Make sure the sound file is there and whole, and say where it is.
    ///
    /// Checked before every play, not written once: the temp folder is
    /// swept (by macOS, by systemd-tmpfiles), and a plotter left running
    /// for a week would otherwise find its alarm file gone — the player
    /// starts, fails without a word, and nothing sounds until a restart.
    pub(super) fn ensure(path: &std::path::Path) -> std::io::Result<()> {
        let wav = super::wav();
        if std::fs::metadata(path).is_ok_and(|m| m.len() == wav.len() as u64) {
            return Ok(());
        }
        std::fs::write(path, wav)
    }

    fn file() -> Option<PathBuf> {
        let path = std::env::temp_dir().join("manx-alarm.wav");
        match ensure(&path) {
            Ok(()) => Some(path),
            Err(e) => {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| log::warn!("alarm sound: cannot write {}: {e}", path.display()));
                None
            }
        }
    }

    fn players() -> &'static [&'static [&'static str]] {
        if cfg!(target_os = "macos") {
            &[&["afplay"]]
        } else {
            &[&["aplay", "-q"], &["paplay"], &["pw-play"]]
        }
    }

    pub fn play() {
        use std::sync::atomic::Ordering;
        let Some(path) = file() else { return };
        let mut playing = PLAYING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((child, which)) = playing.as_mut() {
            match child.try_wait() {
                Ok(None) => return, // still sounding
                // It ran and failed: start with the next one from now on.
                Ok(Some(status)) if !status.success() => {
                    FIRST.store((*which + 1) % players().len(), Ordering::Relaxed);
                }
                _ => {}
            }
        }
        let first = FIRST.load(Ordering::Relaxed);
        let n = players().len();
        for which in (0..n).map(|i| (first + i) % n) {
            let cmd = players()[which];
            let spawned = Command::new(cmd[0])
                .args(&cmd[1..])
                .arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(child) = spawned {
                *playing = Some((child, which));
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
    /// The temp folder is swept from time to time. The file is put back,
    /// whole, whenever it is missing or cut short, and left alone when it
    /// is right.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn a_swept_or_damaged_sound_file_is_put_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alarm.wav");
        super::desktop::ensure(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), super::wav());

        std::fs::remove_file(&path).unwrap();
        super::desktop::ensure(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), super::wav());

        std::fs::write(&path, b"RIFF").unwrap();
        super::desktop::ensure(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), super::wav());
    }

    #[test]
    fn the_alarm_is_a_well_formed_wav() {
        let w = super::wav();
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        let data_len = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
        assert_eq!(w.len(), 44 + data_len);
        // The RIFF size is the file less its first eight bytes; a player that
        // trusts it would otherwise stop short or read past the end.
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize, w.len() - 8);
        let rate = u32::from_le_bytes(w[24..28].try_into().unwrap());
        assert_eq!(rate, 22_050);
        let seconds = data_len as f32 / 2.0 / rate as f32;
        assert!((0.4..0.5).contains(&seconds), "{seconds} s, wanted about half a second");

        // Not silence: a header in front of zeros is a well-formed WAV too,
        // and an alarm nobody hears.
        let samples: Vec<i16> = w[44..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > i16::MAX as u16 / 2, "peak {peak}");
        // And faded in and out, so it does not click.
        assert!(samples[0].unsigned_abs() < 500 && samples[samples.len() - 1].unsigned_abs() < 500);
    }
}
