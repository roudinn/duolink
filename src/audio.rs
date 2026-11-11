//! Game audio without voice chat.
//!
//! Sunshine records everything the host PC plays, including the friend's own
//! voice coming out of Discord, so the guest hears themself. DuoLink records
//! the host instead with Windows process loopback (Windows 10 2004+), leaving
//! out the voice-chat process tree, and sends raw 48 kHz PCM through the
//! tunnel. 5 ms per datagram, ~1.5 Mbit/s: far below the video stream and
//! with no codec delay.

use std::{
    collections::VecDeque,
    mem::size_of,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Media::Audio::{
            AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
            AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
            IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
            IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
            PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
            eConsole, eRender,
        },
        System::{
            Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, StructuredStorage::PROPVARIANT},
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
            },
            Threading::{CreateEventW, GetCurrentProcessId, SetEvent, WaitForSingleObject},
        },
    },
    core::{HRESULT, IUnknown, Interface, Ref, implement},
};

use crate::platform::prioritize_thread;

pub const AUDIO_PORT: u16 = crate::protocol::MEDIA_PORT + 30;
const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// Frames per datagram: 5 ms.
const CHUNK: usize = 240;
const HELLO: &[u8] = b"duolink-audio";
/// Voice-chat programs whose output must not reach the guest, most likely first.
const VOICE: [&str; 6] = [
    "discord.exe",
    "discordptb.exe",
    "discordcanary.exe",
    "ts3client_win64.exe",
    "teamspeak.exe",
    "telegram.exe",
];

fn format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: CHANNELS as u16,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

struct Event(HANDLE);

impl Event {
    fn new() -> Result<Self> {
        Ok(Self(unsafe { CreateEventW(None, false, false, None)? }))
    }
    fn wait(&self, ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

unsafe impl Send for Event {}
unsafe impl Sync for Event {}

/// The root of a running voice-chat process tree, if any.
pub fn voice_app() -> Option<(u32, String)> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()? };
    let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut processes = Vec::new();
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry).is_ok() };
    while more {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
        processes.push((entry.th32ProcessID, entry.th32ParentProcessID, name));
        more = unsafe { Process32NextW(snapshot, &mut entry).is_ok() };
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    VOICE.iter().find_map(|voice| {
        processes
            .iter()
            .filter(|(_, _, name)| name == voice)
            // The tree root: its parent is not another copy of the same program.
            .find(|(_, parent, _)| !processes.iter().any(|(pid, _, name)| pid == parent && name == voice))
            .map(|(pid, _, name)| (*pid, name.trim_end_matches(".exe").to_string()))
    })
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(Arc<Event>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        unsafe { SetEvent(self.0.0) }
    }
}

/// PROPVARIANT holding a VT_BLOB, laid out as on x64.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    reserved: [u16; 3],
    size: u32,
    data: *const AUDIOCLIENT_ACTIVATION_PARAMS,
}

/// Everything the host plays except the `exclude` process tree.
fn loopback_client(exclude: u32) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: exclude,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    let blob = BlobVariant {
        vt: 65, // VT_BLOB
        reserved: [0; 3],
        size: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        data: &params,
    };
    const _: () = assert!(size_of::<BlobVariant>() == size_of::<PROPVARIANT>());
    let done = Arc::new(Event::new()?);
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(done.clone()).into();
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&blob as *const BlobVariant as *const PROPVARIANT),
            &handler,
        )?
    };
    anyhow::ensure!(done.wait(5000), "Windows не ответила на запрос захвата звука.");
    let mut result = HRESULT(0);
    let mut unknown: Option<IUnknown> = None;
    unsafe { operation.GetActivateResult(&mut result, &mut unknown)? };
    result.ok()?;
    Ok(unknown.context("Нет аудиоклиента.")?.cast()?)
}

fn capture(socket: &UdpSocket, guest: SocketAddr, exclude: u32, stop: &AtomicBool) -> Result<()> {
    let client = loopback_client(exclude)?;
    let ready = Event::new()?;
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            100_000, // 10 ms
            0,
            &format(),
            None,
        )?;
        client.SetEventHandle(ready.0)?;
    }
    let reader: IAudioCaptureClient = unsafe { client.GetService()? };
    unsafe { client.Start()? };
    let mut pending: Vec<i16> = Vec::with_capacity(CHUNK * CHANNELS * 4);
    let mut sequence = 0u32;
    let mut datagram = Vec::with_capacity(4 + CHUNK * CHANNELS * 2);
    let mut recheck = Instant::now() + Duration::from_secs(3);
    while !stop.load(Ordering::Relaxed) {
        // Voice chat started or restarted: capture again without it.
        if Instant::now() >= recheck {
            recheck = Instant::now() + Duration::from_secs(3);
            let now = voice_app().map_or(unsafe { GetCurrentProcessId() }, |(pid, _)| pid);
            if now != exclude {
                unsafe {
                    let _ = client.Stop();
                }
                return Ok(());
            }
        }
        ready.wait(20);
        loop {
            let frames = unsafe { reader.GetNextPacketSize()? };
            if frames == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut count = 0u32;
            let mut flags = 0u32;
            unsafe { reader.GetBuffer(&mut data, &mut count, &mut flags, None, None)? };
            let samples = count as usize * CHANNELS;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                pending.resize(pending.len() + samples, 0);
            } else {
                pending.extend_from_slice(unsafe { std::slice::from_raw_parts(data as *const i16, samples) });
            }
            unsafe { reader.ReleaseBuffer(count)? };
        }
        while pending.len() >= CHUNK * CHANNELS {
            datagram.clear();
            datagram.extend_from_slice(&sequence.to_be_bytes());
            for sample in pending.drain(..CHUNK * CHANNELS) {
                datagram.extend_from_slice(&sample.to_le_bytes());
            }
            sequence = sequence.wrapping_add(1);
            let _ = socket.send_to(&datagram, guest);
        }
    }
    unsafe {
        let _ = client.Stop();
    }
    Ok(())
}

/// Host side: records game audio and sends it to the guest.
pub struct HostAudio {
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl HostAudio {
    /// Binds the audio port and checks that process loopback works here.
    pub fn start(address: Ipv4Addr) -> Result<Self> {
        let socket = UdpSocket::bind((address, AUDIO_PORT))
            .with_context(|| format!("UDP-порт {AUDIO_PORT} недоступен."))?;
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        loopback_client(unsafe { GetCurrentProcessId() }).context("Захват звука без голосового чата недоступен.")?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let task = thread::Builder::new().name("duolink-audio".into()).spawn(move || {
            prioritize_thread();
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let mut buffer = [0u8; 64];
            let mut guest = None;
            while !flag.load(Ordering::Relaxed) {
                // The guest says hello through the tunnel; its source is where audio goes.
                if let Ok((count, from)) = socket.recv_from(&mut buffer)
                    && &buffer[..count] == HELLO
                {
                    guest = Some(from);
                }
                let Some(target) = guest else { continue };
                let exclude = voice_app().map_or(unsafe { GetCurrentProcessId() }, |(pid, _)| pid);
                if capture(&socket, target, exclude, &flag).is_err() {
                    thread::sleep(Duration::from_millis(500));
                }
            }
        })?;
        Ok(Self { stop, task: Some(task) })
    }
}

impl Drop for HostAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

/// Samples waiting to be played. Kept short: late audio is dropped, not queued.
struct Jitter {
    samples: VecDeque<i16>,
    last: Option<u32>,
}

const TARGET: usize = 480 * CHANNELS; // 10 ms
const CEILING: usize = 1920 * CHANNELS; // 40 ms

impl Jitter {
    fn push(&mut self, sequence: u32, pcm: &[u8]) {
        if let Some(last) = self.last
            && sequence.wrapping_sub(last) > u32::MAX / 2
        {
            return; // older than what already played
        }
        self.last = Some(sequence);
        self.samples.extend(pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
        if self.samples.len() > CEILING {
            let extra = self.samples.len() - TARGET;
            self.samples.drain(..extra - extra % CHANNELS);
        }
    }
}

fn play(queue: &Mutex<Jitter>, stop: &AtomicBool) -> Result<()> {
    let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let ready = Event::new()?;
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            100_000,
            0,
            &format(),
            None,
        )?;
        client.SetEventHandle(ready.0)?;
    }
    let size = unsafe { client.GetBufferSize()? };
    let writer: IAudioRenderClient = unsafe { client.GetService()? };
    unsafe { client.Start()? };
    let mut primed = false;
    while !stop.load(Ordering::Relaxed) {
        if !ready.wait(100) {
            continue;
        }
        let free = size - unsafe { client.GetCurrentPadding()? };
        if free == 0 {
            continue;
        }
        let data = unsafe { writer.GetBuffer(free)? } as *mut i16;
        let out = unsafe { std::slice::from_raw_parts_mut(data, free as usize * CHANNELS) };
        let mut queue = queue.lock().unwrap();
        // Start only with a little in hand, so one late packet is not a click.
        primed = (primed && !queue.samples.is_empty()) || queue.samples.len() >= TARGET;
        let mut written = 0;
        if primed {
            let take = out.len().min(queue.samples.len());
            for (slot, sample) in out.iter_mut().zip(queue.samples.drain(..take)) {
                *slot = sample;
                written += 1;
            }
        }
        drop(queue);
        out[written..].fill(0);
        unsafe { writer.ReleaseBuffer(free, 0)? };
    }
    unsafe {
        let _ = client.Stop();
    }
    Ok(())
}

/// Guest side: receives the host's audio and plays it on the default device.
pub struct GuestAudio {
    stop: Arc<AtomicBool>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub received: Arc<AtomicU64>,
    tasks: Vec<thread::JoinHandle<()>>,
}

impl GuestAudio {
    /// `silent`: receive only (host and guest on one PC would loop the sound).
    pub fn start(host: SocketAddr, silent: bool) -> Result<Self> {
        let socket = UdpSocket::bind((host.ip(), 0)).context("Не удалось открыть порт для звука.")?;
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let received = Arc::new(AtomicU64::new(0));
        let queue = Arc::new(Mutex::new(Jitter { samples: VecDeque::new(), last: None }));
        let mut tasks = Vec::new();
        {
            let (stop, received, queue) = (stop.clone(), received.clone(), queue.clone());
            tasks.push(thread::Builder::new().name("duolink-audio-in".into()).spawn(move || {
                prioritize_thread();
                let mut buffer = [0u8; 2048];
                let mut hello = Instant::now();
                let _ = socket.send_to(HELLO, host);
                while !stop.load(Ordering::Relaxed) {
                    if hello.elapsed() > Duration::from_secs(1) {
                        let _ = socket.send_to(HELLO, host);
                        hello = Instant::now();
                    }
                    let Ok((count, _)) = socket.recv_from(&mut buffer) else { continue };
                    if count < 4 + CHANNELS * 2 {
                        continue;
                    }
                    received.fetch_add(1, Ordering::Relaxed);
                    if !silent {
                        let sequence = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
                        queue.lock().unwrap().push(sequence, &buffer[4..count]);
                    }
                }
            })?);
        }
        if !silent {
            let stop = stop.clone();
            tasks.push(thread::Builder::new().name("duolink-audio-out".into()).spawn(move || {
                prioritize_thread();
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                // A device change (headphones unplugged) ends play(); open the new default.
                while !stop.load(Ordering::Relaxed) {
                    if play(&queue, &stop).is_err() {
                        thread::sleep(Duration::from_millis(300));
                    }
                }
            })?);
        }
        Ok(Self { stop, received, tasks })
    }
}

impl Drop for GuestAudio {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for task in self.tasks.drain(..) {
            let _ = task.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_drops_old_and_late_audio() {
        let mut jitter = Jitter { samples: VecDeque::new(), last: None };
        let chunk = vec![1u8; CHUNK * CHANNELS * 2];
        jitter.push(10, &chunk);
        jitter.push(9, &chunk);
        assert_eq!(jitter.samples.len(), CHUNK * CHANNELS);
        for sequence in 11..40 {
            jitter.push(sequence, &chunk);
        }
        assert!(jitter.samples.len() <= CEILING);
    }

    #[test]
    fn chime_is_a_valid_wav() {
        let wav = chime();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 2 * (44_100.0f32 * 0.75) as usize);
        if let Some(path) = std::env::var_os("DUOLINK_CHIME_OUT") {
            std::fs::write(path, wav).unwrap();
        }
    }

    #[test]
    #[ignore = "needs an audio device"]
    fn process_loopback_activates() {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap() };
        loopback_client(unsafe { GetCurrentProcessId() }).unwrap();
        eprintln!("voice app: {:?}", voice_app());
    }
}

/// A soft two-note chime for an incoming invite, made here so no file ships.
fn chime() -> &'static [u8] {
    static WAV: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    WAV.get_or_init(|| {
        const SR: u32 = 44_100;
        // Two bell-like notes a fifth apart, the second starting as the first fades.
        let notes = [(880.0f32, 0.0f32), (1318.5, 0.11)];
        let length = (SR as f32 * 0.75) as usize;
        let mut pcm = Vec::with_capacity(length * 2);
        for i in 0..length {
            let t = i as f32 / SR as f32;
            let mut value = 0.0;
            for (frequency, start) in notes {
                let local = t - start;
                if local < 0.0 {
                    continue;
                }
                let attack = (local / 0.006).min(1.0);
                let envelope = attack * (-local * 6.5).exp();
                let phase = std::f32::consts::TAU * frequency * local;
                value += envelope * (phase.sin() + 0.18 * (2.0 * phase).sin());
            }
            let sample = ((value * 0.16).clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let mut wav = Vec::with_capacity(44 + pcm.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&SR.to_le_bytes());
        wav.extend_from_slice(&(SR * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(&pcm);
        wav
    })
}

/// Plays the invite chime without blocking.
pub fn play_chime() {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    let wav = chime();
    unsafe {
        let _ = PlaySoundW(
            windows::core::PCWSTR(wav.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}
