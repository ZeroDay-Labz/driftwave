//! Where the music goes: a sound card, or a virtual microphone other apps
//! (Discord, a softphone…) can record from.
//!
//! The audio stream itself never changes: it is opened once on the default
//! device and then moved around inside PipeWire/PulseAudio with `pactl`, so
//! switching is instant and playback code doesn't know about any of this.
//!
//!   driftwave ──► driftwave_<pid> (null sink) ──► driftwave_mic_<pid> (source)
//!                        │                               ▲ apps record here
//!                        └─► loopback ─► speakers ("hear it too")
//!
//! Everything here blocks on `pactl`, so call it from a background thread.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// Our virtual devices are named `driftwave_<pid>` / `driftwave_mic_<pid>`,
/// so several instances never touch each other's.
const PREFIX: &str = "driftwave_";

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub enum Output {
    /// Wherever the system plays sound.
    #[default]
    Default,
    /// A sound card, by its PipeWire/PulseAudio sink name.
    Device(String),
    /// A microphone other apps can pick as their input.
    VirtualMic,
}

pub struct Sink {
    pub name: String,
    pub description: String,
}

/// An app that is recording right now (in a call, say).
pub struct Recorder {
    pub index: u32,
    pub app: String,
    /// The source it records from.
    source: String,
    /// It is recording our virtual microphone.
    pub sent: bool,
}

pub struct Snapshot {
    pub sinks: Vec<Sink>,
    /// The system's default sink and source (never our own virtual ones).
    pub default_sink: String,
    default_source: String,
    pub recorders: Vec<Recorder>,
    /// Our playback stream and the sink it is on, once the server knows it.
    pub our_input: Option<(u32, u32)>,
}

impl Snapshot {
    pub fn description<'a>(&'a self, sink: &'a str) -> &'a str {
        self.sinks.iter().find(|s| s.name == sink).map_or(sink, |s| s.description.as_str())
    }
}

pub enum Change {
    Apply { output: Output, hear_too: bool, mix_mic: bool },
    /// Point a recording app at the virtual microphone, or back.
    Send { recorder: u32, on: bool },
}

fn sink_name() -> String {
    format!("{PREFIX}{}", std::process::id())
}

fn mic_name() -> String {
    format!("{PREFIX}mic_{}", std::process::id())
}

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl")
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| anyhow!("pactl not found: choosing an output needs PipeWire or PulseAudio"))?;
    if !out.status.success() {
        bail!("pactl {}: {}", args[0], String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Deserialize)]
struct RawSink {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct RawStream {
    index: u32,
    #[serde(default)]
    sink: Option<u32>,
    #[serde(default)]
    source: Option<u32>,
    #[serde(default)]
    properties: HashMap<String, serde_json::Value>,
}

impl RawStream {
    fn prop(&self, key: &str) -> Option<&str> {
        self.properties.get(key)?.as_str()
    }

    fn pid(&self) -> Option<u32> {
        self.prop("application.process.id")?.parse().ok()
    }
}

/// `pactl -f json list sinks`, without our own virtual sinks.
fn parse_sinks(json: &str) -> Result<Vec<Sink>> {
    let raw: Vec<RawSink> = serde_json::from_str(json)?;
    Ok(raw
        .into_iter()
        .filter(|s| !s.name.starts_with(PREFIX))
        .map(|s| Sink { description: if s.description.is_empty() { s.name.clone() } else { s.description }, name: s.name })
        .collect())
}

/// Our playback stream in `pactl -f json list sink-inputs`: (index, sink).
fn parse_our_input(json: &str, pid: u32) -> Result<Option<(u32, u32)>> {
    let raw: Vec<RawStream> = serde_json::from_str(json)?;
    Ok(raw.iter().find(|s| s.pid() == Some(pid)).map(|s| (s.index, s.sink.unwrap_or(u32::MAX))))
}

/// Other apps in `pactl -f json list source-outputs`. Streams without a
/// process (loopbacks, our own remap source) and volume meters are skipped.
fn parse_recorders(json: &str, pid: u32, sources: &[(u32, String)], mic: &str) -> Result<Vec<Recorder>> {
    let raw: Vec<RawStream> = serde_json::from_str(json)?;
    Ok(raw
        .iter()
        .filter(|s| s.pid().is_some_and(|p| p != pid) && s.prop("media.name") != Some("Peak detect"))
        .map(|s| {
            let app = s.prop("application.process.binary").or(s.prop("application.name")).unwrap_or("?");
            let source = sources.iter().find(|(i, _)| Some(*i) == s.source).map_or("", |(_, n)| n.as_str());
            Recorder { index: s.index, app: app.to_string(), source: source.to_string(), sent: source == mic }
        })
        .collect())
}

/// `pactl list short …`: (index, name, the rest of the line).
fn parse_short(text: &str) -> Vec<(u32, String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut cols = line.splitn(3, '\t');
            let index = cols.next()?.trim().parse().ok()?;
            Some((index, cols.next()?.to_string(), cols.next().unwrap_or("").to_string()))
        })
        .collect()
}

/// The pid in a module argument naming one of our virtual devices.
fn owner_pid(args: &str) -> Option<u32> {
    let rest = &args[args.find(PREFIX)? + PREFIX.len()..];
    let rest = rest.strip_prefix("mic_").unwrap_or(rest);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

pub fn snapshot() -> Result<Snapshot> {
    let pid = std::process::id();
    let sinks = parse_sinks(&pactl(&["-f", "json", "list", "sinks"])?)?;
    let sources: Vec<(u32, String)> =
        parse_short(&pactl(&["list", "short", "sources"])?).into_iter().map(|(i, name, _)| (i, name)).collect();
    // While our virtual devices exist one of them may have become the
    // default; "default" here always means a real device.
    let real = |name: String, fallback: Option<&str>| {
        if name.is_empty() || name.starts_with(PREFIX) { fallback.unwrap_or("").to_string() } else { name }
    };
    let default_sink = real(
        pactl(&["get-default-sink"]).unwrap_or_default().trim().to_string(),
        sinks.first().map(|s| s.name.as_str()),
    );
    let microphone = sources.iter().map(|(_, n)| n.as_str()).find(|n| !n.starts_with(PREFIX) && !n.ends_with(".monitor"));
    let default_source = real(pactl(&["get-default-source"]).unwrap_or_default().trim().to_string(), microphone);
    Ok(Snapshot {
        recorders: parse_recorders(&pactl(&["-f", "json", "list", "source-outputs"])?, pid, &sources, &mic_name())?,
        our_input: parse_our_input(&pactl(&["-f", "json", "list", "sink-inputs"])?, pid)?,
        sinks,
        default_sink,
        default_source,
    })
}

fn load(args: &[&str]) -> Result<u32> {
    let mut full = vec!["load-module"];
    full.extend(args);
    let out = pactl(&full)?;
    out.trim().parse().map_err(|_| anyhow!("pactl load-module {}: unexpected reply {out:?}", args[0]))
}

fn unload(module: u32) {
    let _ = pactl(&["unload-module", &module.to_string()]);
}

/// The virtual devices we created and the streams we moved, so it can all
/// be undone. Dropping it undoes it.
#[derive(Default)]
pub struct Routing {
    null: Option<u32>,
    mic: Option<u32>,
    /// Loopback modules and what they connect to.
    hear: Option<(u32, String)>,
    mix: Option<(u32, String)>,
    /// Recording apps we pointed at the microphone, and where they were.
    moved: Vec<(u32, String)>,
}

/// Make `slot` a loopback to/from `want` (or nothing).
fn relink(slot: &mut Option<(u32, String)>, want: Option<String>, args: impl Fn(&str) -> [String; 2]) -> Result<()> {
    if slot.as_ref().map(|(_, to)| to) == want.as_ref() {
        return Ok(());
    }
    if let Some((module, _)) = slot.take() {
        unload(module);
    }
    if let Some(to) = want {
        let [source, sink] = args(&to);
        *slot = Some((load(&["module-loopback", &source, &sink])?, to));
    }
    Ok(())
}

impl Routing {
    /// Returns a note for the user when the choice couldn't be honoured.
    fn apply(&mut self, want: &Output, hear_too: bool, mix_mic: bool, snap: &Snapshot) -> Result<Option<String>> {
        let Some((input, _)) = snap.our_input else { return Ok(None) };
        let input = input.to_string();
        match want {
            Output::VirtualMic => {
                let (sink, mic) = (sink_name(), mic_name());
                if self.null.is_none() {
                    let name = format!("sink_name={sink}");
                    self.null = Some(load(&["module-null-sink", &name, "sink_properties=device.description=driftwave"])?);
                }
                if self.mic.is_none() {
                    // Apps like Discord hide ".monitor" sources, so give
                    // them a proper one.
                    let (master, name) = (format!("master={sink}.monitor"), format!("source_name={mic}"));
                    self.mic = Some(load(&[
                        "module-remap-source",
                        &master,
                        &name,
                        "source_properties=device.description=driftwave",
                    ])?);
                }
                pactl(&["move-sink-input", &input, &sink])?;
                let nonempty = |s: &String| !s.is_empty();
                let speakers = hear_too.then(|| snap.default_sink.clone()).filter(nonempty);
                relink(&mut self.hear, speakers, |to| [format!("source={sink}.monitor"), format!("sink={to}")])?;
                let voice = mix_mic.then(|| snap.default_source.clone()).filter(nonempty);
                relink(&mut self.mix, voice, |from| [format!("source={from}"), format!("sink={sink}")])?;
                Ok(None)
            }
            Output::Device(name) if snap.sinks.iter().any(|s| s.name == *name) => {
                pactl(&["move-sink-input", &input, name])?;
                self.drop_virtual();
                Ok(None)
            }
            other => {
                pactl(&["move-sink-input", &input, "@DEFAULT_SINK@"])?;
                self.drop_virtual();
                Ok(match other {
                    Output::Device(name) => Some(format!("{name} isn't connected; playing on the system default")),
                    _ => None,
                })
            }
        }
    }

    fn send(&mut self, recorder: u32, on: bool, snap: &Snapshot) -> Result<()> {
        let index = recorder.to_string();
        let known = self.moved.iter().position(|(i, _)| *i == recorder);
        if on {
            if self.mic.is_none() {
                bail!("Choose the virtual microphone first");
            }
            pactl(&["move-source-output", &index, &mic_name()])?;
            if known.is_none() {
                let was = snap.recorders.iter().find(|r| r.index == recorder).map(|r| r.source.clone());
                self.moved.push((recorder, was.unwrap_or_default()));
            }
        } else {
            let was = known.map(|i| self.moved.remove(i).1).filter(|s| !s.is_empty());
            pactl(&["move-source-output", &index, was.as_deref().unwrap_or("@DEFAULT_SOURCE@")])?;
        }
        Ok(())
    }

    /// Put recording apps back and remove our virtual devices.
    fn drop_virtual(&mut self) {
        for (recorder, was) in self.moved.drain(..) {
            let was = if was.is_empty() { "@DEFAULT_SOURCE@".to_string() } else { was };
            let _ = pactl(&["move-source-output", &recorder.to_string(), &was]);
        }
        let loopbacks = [self.hear.take(), self.mix.take()].into_iter().flatten().map(|(m, _)| m);
        for module in loopbacks.chain(self.mic.take()).chain(self.null.take()) {
            unload(module);
        }
    }
}

impl Drop for Routing {
    fn drop(&mut self) {
        self.drop_virtual();
    }
}

/// Look at the sound server, after making `change` if there is one.
pub fn sync(routing: &Mutex<Routing>, change: Option<Change>) -> Result<(Snapshot, Option<String>)> {
    let snap = snapshot()?;
    let Some(change) = change else { return Ok((snap, None)) };
    let mut routing = routing.lock().unwrap();
    let note = match change {
        Change::Apply { output, hear_too, mix_mic } => routing.apply(&output, hear_too, mix_mic, &snap)?,
        Change::Send { recorder, on } => {
            routing.send(recorder, on, &snap)?;
            None
        }
    };
    Ok((snapshot()?, note))
}

/// Remove virtual devices left behind by an instance that was killed.
pub fn sweep_stale() {
    let Ok(modules) = pactl(&["list", "short", "modules"]) else { return };
    for (index, _, args) in parse_short(&modules) {
        if let Some(pid) = owner_pid(&args)
            && !std::path::Path::new(&format!("/proc/{pid}")).exists()
        {
            unload(index);
        }
    }
}

/// One line of the Audio settings tab.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Row {
    Heading(&'static str),
    Note(&'static str),
    Default,
    /// Index into `Snapshot::sinks`.
    Device(usize),
    /// The saved device, which isn't connected right now.
    Missing,
    VirtualMic,
    HearToo,
    MixMic,
    /// Index into `Snapshot::recorders`.
    App(usize),
}

impl Row {
    pub fn selectable(self) -> bool {
        !matches!(self, Row::Heading(_) | Row::Note(_))
    }
}

pub fn rows(snap: &Snapshot, current: &Output) -> Vec<Row> {
    let mut rows = vec![Row::Heading("Play through"), Row::Default];
    rows.extend((0..snap.sinks.len()).map(Row::Device));
    if matches!(current, Output::Device(name) if !snap.sinks.iter().any(|s| s.name == *name)) {
        rows.push(Row::Missing);
    }
    rows.push(Row::VirtualMic);
    if *current == Output::VirtualMic {
        rows.extend([Row::HearToo, Row::MixMic, Row::Heading("Send to an app that is recording now")]);
        if snap.recorders.is_empty() {
            rows.push(Row::Note("(none: start a call, or pick \"driftwave\" as the app's input device)"));
        }
        rows.extend((0..snap.recorders.len()).map(Row::App));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINKS: &str = r#"[
        {"index":543,"state":"SUSPENDED","name":"alsa_output.pci-0000_10_00.4.analog-stereo",
         "description":"Starship/Matisse HD Audio Controller Analog Stereo","properties":{"device.bus":"pci"}},
        {"index":546,"name":"alsa_output.usb-Headset.analog-stereo","description":"Headset Analog Stereo"},
        {"index":600,"name":"driftwave_4242","description":"driftwave"}
    ]"#;

    const SINK_INPUTS: &str = r#"[
        {"index":652,"client":"651","sink":546,"properties":{"application.name":"speech-dispatcher-dummy",
         "application.process.id":"30299","application.process.binary":"sd_dummy"}},
        {"index":700,"sink":543,"properties":{"application.name":"ALSA plug-in [driftwave]",
         "application.process.id":"4242","application.process.binary":"driftwave"}},
        {"index":701,"owner_module":"12","sink":546,"properties":{"media.name":"loopback"}}
    ]"#;

    const SOURCE_OUTPUTS: &str = r#"[
        {"index":772,"owner_module":null,"source":547,"properties":{"application.name":"WEBRTC VoiceEngine",
         "application.process.id":"30012","application.process.binary":"Discord","media.name":"recStream"}},
        {"index":773,"source":601,"properties":{"application.name":"oxidesip","application.process.id":"555"}},
        {"index":774,"source":600,"properties":{"media.name":"remap"}},
        {"index":775,"source":547,"properties":{"application.process.id":"9","application.process.binary":"pavucontrol",
         "media.name":"Peak detect"}}
    ]"#;

    #[test]
    fn reads_pactl_output() {
        let sinks = parse_sinks(SINKS).unwrap();
        let names: Vec<&str> = sinks.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names.len(), 2, "our own virtual sink isn't offered: {names:?}");
        assert_eq!(sinks[1].description, "Headset Analog Stereo");

        assert_eq!(parse_our_input(SINK_INPUTS, 4242).unwrap(), Some((700, 543)));
        assert_eq!(parse_our_input(SINK_INPUTS, 1).unwrap(), None);

        let sources = vec![(547, "alsa_input.usb-Headset.mono".to_string()), (601, "driftwave_mic_4242".to_string())];
        let recorders = parse_recorders(SOURCE_OUTPUTS, 4242, &sources, "driftwave_mic_4242").unwrap();
        let seen: Vec<(u32, &str, bool)> = recorders.iter().map(|r| (r.index, r.app.as_str(), r.sent)).collect();
        assert_eq!(seen, [(772, "Discord", false), (773, "oxidesip", true)], "loopbacks and meters are skipped");
        assert_eq!(recorders[0].source, "alsa_input.usb-Headset.mono");

        let modules = "12\tmodule-null-sink\tsink_name=driftwave_4242 sink_properties=device.description=driftwave\n\
                       13\tmodule-remap-source\tmaster=driftwave_4242.monitor source_name=driftwave_mic_4242\n\
                       14\tmodule-loopback\tsource=alsa_input.usb sink=alsa_output.usb\n\
                       15\tlibpipewire-module-rt\n";
        let owners: Vec<(u32, Option<u32>)> = parse_short(modules).iter().map(|(i, _, a)| (*i, owner_pid(a))).collect();
        assert_eq!(owners, [(12, Some(4242)), (13, Some(4242)), (14, None), (15, None)]);
        assert_eq!(owner_pid("source_name=driftwave_mic_77"), Some(77));
    }

    #[test]
    fn audio_tab_rows() {
        let snap = Snapshot {
            sinks: parse_sinks(SINKS).unwrap(),
            default_sink: "alsa_output.usb-Headset.analog-stereo".into(),
            default_source: String::new(),
            recorders: Vec::new(),
            our_input: None,
        };
        assert_eq!(snap.description(&snap.default_sink), "Headset Analog Stereo");
        let plain = rows(&snap, &Output::Default);
        assert_eq!(plain[1..], [Row::Default, Row::Device(0), Row::Device(1), Row::VirtualMic]);
        assert!(!plain[0].selectable() && plain[1].selectable());
        // An unplugged saved device stays visible; the microphone's options
        // only show while it is chosen.
        assert!(rows(&snap, &Output::Device("gone".into())).contains(&Row::Missing));
        let mic = rows(&snap, &Output::VirtualMic);
        assert!(mic.contains(&Row::HearToo) && mic.contains(&Row::MixMic) && !mic.contains(&Row::Missing));

        let saved = serde_json::to_string(&Output::Device("x".into())).unwrap();
        assert_eq!(serde_json::from_str::<Output>(&saved).unwrap(), Output::Device("x".into()));
    }
}
