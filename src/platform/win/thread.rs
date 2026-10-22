use windows::core::PCWSTR;
use windows::Win32::System::Threading::{
    AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, GetCurrentThread, SetThreadPriority,
    AVRT_PRIORITY_HIGH, THREAD_PRIORITY_HIGHEST,
};

/// Puts the calling thread under the "Pro Audio" multimedia scheduler, the same task name the audio
/// engine itself uses. Falls back to a plain high thread priority when the scheduler is unavailable;
/// neither path can fail the stream, so this never returns an error.
pub fn enter_audio_thread() {
    let name: Vec<u16> = "Pro Audio"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut index = 0u32;
    let task = unsafe { AvSetMmThreadCharacteristicsW(PCWSTR(name.as_ptr()), &mut index) };
    match task {
        Ok(handle) => {
            let _ = unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_HIGH) };
            // The task handle is bound to this thread and lives until the process exits; reverting it
            // would only matter for threads that are recycled, which these are not.
        }
        Err(_) => {
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
        }
    }
}
