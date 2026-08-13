use clap::Parser;
use env_logger::Env;
use log::{self, error, info, warn};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, process::exit};

use gst::prelude::*;
use gstreamer::{
    self as gst, DebugGraphDetails, ElementFactory, GhostPad, PadDirection, PadProbeType,
};

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// WHEP source url
    #[clap(short, long)]
    pub input_url: String,

    /// SRT output stream url
    #[clap(short, long, default_value_t = String::from("srt://0.0.0.0:1234?mode=listener"))]
    pub output_url: String,

    /// Output debug .dot files
    #[clap(long)]
    pub dot_debug: bool,

    /// Jitterbuffer latency in milliseconds (sets rtpbin latency and liveadder min-upstream-latency).
    /// If unset, defaults to 200 for audio-only setups and 2000 when `--bridge-video` is enabled —
    /// WebRTC video sources commonly exhibit 1–2 s RTP clock skew, which combined with
    /// `drop-on-latency=true` would otherwise discard IDR packets and leave subscribers without a
    /// decode entry point.
    #[clap(long, env = "WHEP_SRT_JITTERBUFFER_LATENCY")]
    pub latency: Option<u64>,

    /// Authorization token for WHEP endpoint
    #[clap(long, env = "WHEP_SRT_AUTH_TOKEN")]
    pub auth_token: Option<String>,

    /// Bridge video tracks from WHEP to SRT output (transcodes to H.264 in MPEG-TS by default;
    /// pass `--passthrough` to skip re-encoding when the source is already H.264).
    #[clap(long)]
    pub bridge_video: bool,

    /// Skip the decode/re-encode step when the WHEP source is already H.264 — passes the
    /// source bitstream straight through to MPEG-TS. Saves a generation of encoding loss
    /// and ~1 core of CPU, but exposes the subscriber to all source-side flakiness:
    /// variable bitrate, sparse keyframes, RTP packet loss → visible decode corruption.
    /// Default is to transcode regardless of source codec, which is more robust for
    /// arbitrary WebRTC publishers. Only effective when --bridge-video is also set and
    /// the source negotiates H.264; non-H.264 sources always transcode.
    #[clap(long)]
    pub passthrough: bool,

    /// x264enc bitrate in kbps (used when --bridge-video is set)
    #[clap(long, env = "WHEP_SRT_VIDEO_BITRATE", default_value_t = 8000)]
    pub video_bitrate: u32,

    /// x264enc speed-preset: ultrafast, superfast, veryfast, faster, fast, medium, slow,
    /// slower, veryslow, placebo.  Slower presets give better quality at the same bitrate
    /// but use more CPU; veryfast/faster are usually safe for live, fast and above need
    /// strong hardware for 720p+/30fps.
    #[clap(long, env = "WHEP_SRT_VIDEO_PRESET", default_value_t = String::from("fast"))]
    pub video_preset: String,

    /// x264enc key-int-max: max distance between keyframes (in frames).  Smaller values
    /// give faster initial sync for new viewers but worse compression efficiency.
    #[clap(long, env = "WHEP_SRT_VIDEO_KEY_INT", default_value_t = 60)]
    pub video_key_int: u32,

    /// How often to send an upstream PLI to the WHEP publisher (in ms) while at least one
    /// SRT subscriber is connected. Browser WebRTC senders emit IDRs only on demand, so
    /// without periodic PLI an SRT consumer joining mid-stream may wait tens of seconds
    /// for a decode entry point. Only applies when --bridge-video is set and only fires
    /// while subscriber count > 0. Set to 0 to disable.
    #[clap(long, env = "WHEP_SRT_VIDEO_PLI_INTERVAL_MS", default_value_t = 2000)]
    pub video_pli_interval_ms: u64,
}

/// Length of an RTP packet's payload, skipping the CSRC list and any header extension
/// (RFC 3550 §5.1). `None` when the buffer is too short to be a valid RTP packet.
fn rtp_payload_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 12 {
        return None;
    }
    let csrc_count = (bytes[0] & 0x0f) as usize;
    let mut offset = 12 + 4 * csrc_count;
    if bytes[0] & 0x10 != 0 {
        if bytes.len() < offset + 4 {
            return None;
        }
        let ext_words = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        offset += 4 + 4 * ext_words;
    }
    Some(bytes.get(offset..)?.len())
}

/// Whether an RTP packet is evidence that its stream carries real media, as opposed to being a
/// bandwidth-probing stream.
///
/// Needed because SMB hands a WHEP consumer one padding-only stream per session alongside the
/// real video, on the same payload type and the same `a-mid`, so the caps cannot tell them apart.
///
/// Two signals, either sufficient. An unpadded packet (P bit clear) is ordinary media: measured
/// over a full session the real video track set P on 0 of 4891 packets while the probing track
/// set it on 293 of 293. A marker bit is the other tell — it terminates a video frame, and the
/// probing track never sets one — which keeps this from misjudging a real sender that pads its
/// media for rate control.
///
/// Note the padding *length* cannot be used here: the RFC 3550 pad count is a single byte, so a
/// payload over 255 bytes is never "all padding" by that measure even when it carries no media,
/// which is exactly the shape these packets have (measured: 97..1169 bytes).
fn rtp_carries_media(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && (bytes[0] & 0x20 == 0 || bytes[1] & 0x80 != 0)
}

/// Build an RTCP REMB packet (draft-alvestrand-rmcat-remb) advertising `bitrate_bps` as the
/// receiver's available bandwidth for `media_ssrcs`.
///
/// Why this is needed at all: SMB decides how much to send an endpoint from the REMB that endpoint
/// reports — `transport/TransportImpl.cpp` assigns `remb.getBitrate()` straight to
/// `_outboundMetrics.estimatedKbps`, with no validation or ramp. Browsers (libwebrtc) send REMB, so
/// they get full rate; GStreamer has no REMB support at all (1.26 `rtpsession` exposes only
/// `twcc-stats`), so this bridge never reports anything and SMB leaves it pinned at
/// `rctl.initialEstimate`. Measured effect: SMB logs `remb 0kbps` for this endpoint and forwards
/// roughly half of a 720p25 stream — ~13 fps with slices missing, which decodes into the smearing
/// on movement, on an otherwise idle network.
///
/// This is an assertion, not a measurement, so it is opt-in: it is only honest on a link whose
/// capacity is actually known, such as a wired LAN between bridge and SFU. Enabled with
/// `WHEP_SRT_REMB_KBPS`.
fn build_remb(sender_ssrc: u32, media_ssrcs: &[u32], bitrate_bps: u64) -> Vec<u8> {
    // REMB encodes the bitrate as an 18-bit mantissa scaled by a 6-bit exponent, so shift the
    // mantissa down until it fits. Rounding down keeps the claim conservative.
    let mut mantissa = bitrate_bps;
    let mut exponent: u32 = 0;
    while mantissa > 0x3_FFFF {
        mantissa >>= 1;
        exponent += 1;
    }
    let mantissa = mantissa as u32;

    let n = media_ssrcs.len().min(255);
    let mut packet = Vec::with_capacity(20 + 4 * n);

    // V=2, P=0, FMT=15 (application-layer feedback); PT=206 (PSFB).
    packet.push(0x8F);
    packet.push(0xCE);
    // Length in 32-bit words minus one: header + sender + media + "REMB" + br + one word per SSRC.
    let words = 5 + n as u16;
    packet.extend_from_slice(&(words - 1).to_be_bytes());
    packet.extend_from_slice(&sender_ssrc.to_be_bytes());
    // Media source SSRC is unused for REMB and must be zero.
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(b"REMB");
    packet.push(n as u8);
    packet.push(((exponent as u8) << 2) | ((mantissa >> 16) & 0x03) as u8);
    packet.push(((mantissa >> 8) & 0xFF) as u8);
    packet.push((mantissa & 0xFF) as u8);
    for ssrc in media_ssrcs.iter().take(n) {
        packet.extend_from_slice(&ssrc.to_be_bytes());
    }
    packet
}

fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let whep_url = args.input_url;
    let output_url = args.output_url;
    let dot_debug = args.dot_debug;
    let bridge_video = args.bridge_video;
    let passthrough = args.passthrough;
    let latency = args
        .latency
        .unwrap_or(if bridge_video { 2000 } else { 200 });
    let video_bitrate = args.video_bitrate;
    let video_preset = args.video_preset;
    let video_key_int = args.video_key_int;
    let video_pli_interval_ms = args.video_pli_interval_ms;
    // DIAGNOSTIC: when set to a directory path, the transcode path writes the
    // DECODED video frames (before any re-encode) as JPEGs there so we can see
    // exactly what the bridge receives & decodes, independent of x264/mux/SRT.
    let dump_frames = std::env::var("WHEP_SRT_DUMP_FRAMES").ok();
    // DIAGNOSTIC: per-SSRC RTP and jitterbuffer statistics, logged every 5 seconds. Off by
    // default — it emits a line per SSRC per interval, which is far too noisy for a normal run,
    // but it is the tool for questions about which stream is which on the wire.
    let rtp_diag = std::env::var("WHEP_SRT_RTP_DIAG").as_deref() == Ok("1");
    // Advertise this many kbps to the SFU as available downlink bandwidth, via RTCP REMB. Off
    // unless set. See build_remb for why it exists and why it is not a default.
    let remb_kbps: Option<u64> = std::env::var("WHEP_SRT_REMB_KBPS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|kbps| *kbps > 0);

    if dot_debug {
        let current_dir = format!(
            "{}",
            env::current_dir()
                .expect("could not get current directory")
                .display()
        );

        log::info!("Debugging .dot files to '{current_dir}'");
        unsafe {
            env::set_var("GST_DEBUG_DUMP_DOT_DIR", current_dir);
        }
    }

    gst::init().expect("Could not initiate GStreamer");

    info!("SRT output at {output_url}");
    if bridge_video {
        if passthrough {
            info!(
                "video bridging enabled: H.264 passthrough opt-in active — H.264 sources pass through \
                 untouched, other codecs transcode to H.264 via x264 (preset={video_preset}, \
                 bitrate={video_bitrate} kbps, key-int-max={video_key_int}, tune=zerolatency)"
            );
        } else {
            info!(
                "video bridging enabled: transcode mode (default) — all sources decode then re-encode \
                 to H.264 via x264 (preset={video_preset}, bitrate={video_bitrate} kbps, \
                 key-int-max={video_key_int}, tune=zerolatency). Pass --passthrough to skip re-encoding \
                 for H.264 sources."
            );
        }
    }
    info!("---");

    /*  NOTE:
       whepsrc was the first WHIP implementation in gstreamer, based on webrtcbin directly. it's present in gstwebrtchttp plugin.
       whepclientsrc has later been added and it is using the signaller interface on webrtcsrc and webrtcsink rust plugins. it's present in gstrswebrtc plugin.
       whepclientsrc is reusing a lot of functionallity and is supposed to deprecate whepsrc in the future.

       In this project we use the new whepclientsrc. Below is some dev code if you want to try out the old whepsrc implementation for some reason.
    */

    let use_whepsrc = false;
    let input = if use_whepsrc {
        //gstwebrtchttp::plugin_register_static().expect("Could not register gstwebrtchttp plugins");

        let audio_caps = "audio_caps=\"application/x-rtp, media=(string)audio, encoding-name=(string)opus, payload=(int)96, encoding-params=(string)2, clock-rate=(int)48000\"";
        format!(
            "whepsrc name=input use-link-headers=false whep-endpoint=\"{whep_url}\" {audio_caps} video-caps=\"\""
        )
    } else {
        gstrswebrtc::plugin_register_static().expect("Could not register gstrswebrtc plugins");

        let mut src = format!("whepclientsrc name=input signaller::whep-endpoint=\"{whep_url}\"");
        if let Some(ref token) = args.auth_token {
            src.push_str(&format!(" signaller::auth-token=\"{token}\""));
        }
        src
    };

    let mixer = "liveadder name=mixer"; //this could be audiomixer also, but liveadder will do fine here

    // Note on the initial PMT: with --bridge-video, the real-video chain attaches to
    // mpegtsmux dynamically when WHEP delivers a video pad (some seconds after startup).
    // Until then the streamheader PAT/PMT advertises audio only.  SRT relays that latch
    // onto the initial streamheaders therefore expose audio only to subscribers.  We
    // previously tried a videotestsrc-fed input-selector to inject video into the initial
    // PMT, but mpegtsmux's aggregator stalls on the running-time discontinuity that comes
    // with the active-pad switch — better to keep the pipeline simple and document the
    // limitation than to ship a stream that hangs.
    let pipeline_str = format!(
        "{input} audiotestsrc wave=silence is-live=true ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! {mixer} ! avenc_aac ! aacparse ! mux. \
        mpegtsmux name=mux alignment=7 ! queue name=srt_queue ! srtsink name=srt_sink uri=\"{output_url}\" sync=false wait-for-connection=false latency=100"
    );

    let mut context = gst::ParseContext::new();
    let pipeline = match gst::parse::launch_full(
        &pipeline_str,
        Some(&mut context),
        gst::ParseFlags::empty(),
    ) {
        Ok(pipeline) => pipeline,
        Err(err) => {
            if let Some(gst::ParseError::NoSuchElement) = err.kind::<gst::ParseError>() {
                error!("Missing element(s): {:?}", context.missing_elements());
            } else {
                error!("Failed to parse pipeline: {err}");
            }

            std::process::exit(-1)
        }
    };

    let pipeline = pipeline
        .dynamic_cast::<gst::Pipeline>()
        .expect("could not cast pipeline");
    let pipeline_clone = pipeline.clone();

    let mixer = pipeline
        .by_name("mixer")
        .expect("could not find mixer element");
    mixer.set_property_from_str("min-upstream-latency", &format!("{latency}000000"));
    let mixer_clone = mixer.clone();

    if bridge_video {
        // PMT-patcher buffer probe.  mpegtsmux always writes an HDMV registration descriptor for
        // H.264 streams.  Without an accompanying AVCDecoderConfigurationRecord (which mpegtsmux
        // only adds when input is AVC, not byte-stream), FFmpeg >= 8 enters HDMV mode and refuses
        // to read inline SPS/PPS — subscribers fail with "non-existing PPS 0 referenced".  Patch
        // the PMT in flight on srt_queue.src: zero out the "HDMV" identifier and recompute the
        // section CRC.  The probe must handle both Buffer and BufferList because `queue`
        // aggregates buffers into BufferLists when downstream (srtsink) supports them.
        let srt_queue = pipeline
            .by_name("srt_queue")
            .expect("could not find srt_queue element");
        let pmt_patcher = Mutex::new(PmtPatcher::new());
        srt_queue
            .static_pad("src")
            .expect("srt_queue has no src pad")
            .add_probe(
                PadProbeType::BUFFER | PadProbeType::BUFFER_LIST,
                move |_pad, probe_info| {
                    let mut patcher = pmt_patcher.lock().unwrap();
                    match probe_info.data {
                        Some(gst::PadProbeData::Buffer(ref mut buf)) => {
                            let buf_mut = buf.make_mut();
                            if let Ok(mut map) = buf_mut.map_writable() {
                                patcher.process(map.as_mut_slice());
                            }
                        }
                        Some(gst::PadProbeData::BufferList(ref mut list)) => {
                            let list_mut = list.make_mut();
                            for i in 0..list_mut.len() {
                                if let Some(buf) = list_mut.get_mut(i) {
                                    if let Ok(mut map) = buf.map_writable() {
                                        patcher.process(map.as_mut_slice());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    gst::PadProbeReturn::Ok
                },
            );

        // Keyframe-on-subscribe.  When a new SRT subscriber connects, ask the WHEP source for a
        // fresh IDR so the subscriber has a decode entry point.  Without this, browser/WebRTC
        // encoders typically only emit an IDR at the start of the stream — anyone connecting
        // later sees nothing but P-frames and decode fails.  The force-key-unit upstream event
        // propagates queue → h264parse → depay → webrtcbin, which translates it to an RTCP PLI
        // on the peer connection.
        //
        // We fire the event as a small burst (0/500/1500 ms) rather than once, for two reasons:
        // some WebRTC senders rate-limit PLIs and ignore the second of two close together, and
        // the rtpjitterbuffer (with drop-on-latency=true) may discard the resulting IDR if it
        // arrives during a skew reset.  Spreading the requests across ~1.5 s gives at least one
        // a good chance of producing an IDR that survives all the way to the subscriber.
        let srt_sink = pipeline
            .by_name("srt_sink")
            .expect("could not find srt_sink element");

        // Track active SRT subscribers so the periodic PLI loop below only runs cost on the
        // publisher while someone is actually downstream. caller-added/-removed fire on srtsink
        // in listener mode; we also use the count for diagnostic logging.
        let subscriber_count = Arc::new(AtomicUsize::new(0));

        let pipeline_for_keyframe = pipeline.clone();
        let subscriber_count_added = Arc::clone(&subscriber_count);
        srt_sink.connect("caller-added", false, move |_values| {
            let n = subscriber_count_added.fetch_add(1, Ordering::SeqCst) + 1;
            info!("SRT subscriber connected (count={n}) — sending PLI burst");
            let pipeline_for_thread = pipeline_for_keyframe.clone();
            std::thread::spawn(move || {
                for (idx, &delay_ms) in [0u64, 500, 1500].iter().enumerate() {
                    if delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                    match pipeline_for_thread.by_name("video_queue") {
                        Some(video_queue) => {
                            let s = gst::Structure::builder("GstForceKeyUnit")
                                .field("all-headers", true)
                                .build();
                            let sent = video_queue
                                .send_event(gst::event::CustomUpstream::new(s));
                            info!("PLI burst {idx} (+{delay_ms}ms): sent={sent}");
                        }
                        None => {
                            info!("PLI burst {idx} (+{delay_ms}ms): no video chain yet");
                        }
                    }
                }
            });
            None
        });

        let subscriber_count_removed = Arc::clone(&subscriber_count);
        srt_sink.connect("caller-removed", false, move |_values| {
            // Defensive saturating decrement: caller-removed should never fire when count==0,
            // but underflowing an AtomicUsize would wedge the periodic PLI loop on permanently.
            let prev = subscriber_count_removed
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    Some(n.saturating_sub(1))
                })
                .unwrap_or(0);
            info!("SRT subscriber disconnected (count={})", prev.saturating_sub(1));
            None
        });

        // Periodic upstream PLI. Browser WebRTC publishers emit IDRs only on demand
        // (NACK/PLI/FIR or stream-start/reconfigure), so without this an SRT subscriber
        // joining mid-stream waits up to the next natural IDR — observed in the wild
        // at ~30 s gaps. The thread runs forever and gates on the live subscriber count
        // so we don't bill the publisher when no one is listening.
        if video_pli_interval_ms > 0 {
            let pipeline_for_periodic = pipeline.clone();
            let subscriber_count_timer = Arc::clone(&subscriber_count);
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_millis(video_pli_interval_ms));
                if subscriber_count_timer.load(Ordering::SeqCst) == 0 {
                    continue;
                }
                if let Some(video_queue) = pipeline_for_periodic.by_name("video_queue") {
                    let s = gst::Structure::builder("GstForceKeyUnit")
                        .field("all-headers", true)
                        .build();
                    let sent =
                        video_queue.send_event(gst::event::CustomUpstream::new(s));
                    log::debug!("periodic PLI sent={sent}");
                }
            });
            info!(
                "periodic upstream PLI enabled: interval={video_pli_interval_ms}ms (gated on SRT subscriber count)"
            );
        } else {
            info!("periodic upstream PLI disabled (interval=0)");
        }
    }

    // Guard: exactly one video track is bridged; additional video tracks go to fakesink.
    let video_bridged = Arc::new(AtomicBool::new(false));

    // Names of pads whose caps have confirmed them as video tracks, for logging and for the
    // watchdog below. This source (SMB) exposes more than one video track — a high-rate stream
    // and a low-rate one, both payload 97 H.264 on a-mid=video0 with different SSRCs — alongside
    // the OPUS audio track, so it is worth recording which ones were seen and which was bridged.
    let video_pad_names: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
    // How long to wait before concluding that no video is going to reach the output.
    const VIDEO_SELECT_FALLBACK: Duration = Duration::from_secs(5);

    let input_whep_bin = pipeline
        .by_name("input")
        .expect("could not get whep input bin");

    let _ = ctrlc::set_handler(move || {
        info!("exit.. shutting down");

        pipeline_clone
            .set_state(gst::State::Null)
            .expect("Unable to set the pipeline to the `Null` state");

        std::thread::sleep(std::time::Duration::from_secs(1));

        exit(0);
    });

    let bus = pipeline.bus().unwrap();

    let pipeline_clone = pipeline.clone();
    // SSRCs we are actually receiving, for REMB to report on. Declared here because the rtpbin hook
    // below needs it, and kept separate from the [pt-diag] tally because that is gated on
    // WHEP_SRT_RTP_DIAG whereas REMB has to work with diagnostics off.
    let observed_ssrcs: Arc<Mutex<BTreeSet<u32>>> = Arc::new(Mutex::new(BTreeSet::new()));
    let observed_ssrcs_for_remb = Arc::clone(&observed_ssrcs);

    pipeline.connect_deep_element_added(move |pipe, bin, elem| {
        let elem_type = elem.type_().to_string();
        let _ = pipe;
        let _ = bin;

        if elem_type == "GstRtpBin" {
            // Append REMB to this session's outgoing RTCP. Piggybacking on the receiver reports
            // rtpbin already sends means no extra timer and no separate socket, and it repeats at
            // the RTCP interval — which matters because an estimate the SFU never hears again may
            // decay. Injected here, before webrtcbin's SRTP encryption, so it is protected like
            // any other RTCP.
            //
            // Appending is safe without parsing: an RTCP compound packet is just concatenated
            // packets, so a well-formed REMB placed after the existing ones is still valid.
            if let Some(kbps) = remb_kbps {
                let observed_ssrcs = Arc::clone(&observed_ssrcs_for_remb);
                elem.connect_pad_added(move |_bin, pad| {
                    if !pad.name().starts_with("send_rtcp_src") {
                        return;
                    }
                    info!(
                        "[remb] advertising {kbps} kbps to the SFU on pad '{}'",
                        pad.name()
                    );
                    let observed_ssrcs = Arc::clone(&observed_ssrcs);
                    let logged = AtomicBool::new(false);
                    pad.add_probe(PadProbeType::BUFFER, move |_pad, probe_info| {
                        let Some(gst::PadProbeData::Buffer(ref buffer)) = probe_info.data else {
                            return gst::PadProbeReturn::Ok;
                        };
                        // Sent with an empty SSRC list until media arrives. Waiting for a known
                        // SSRC would deadlock: the SFU forwards nothing until it hears an
                        // estimate, so no SSRC would ever be observed to name in the report.
                        // SMB reads the bitrate irrespective of the list.
                        let ssrcs: Vec<u32> =
                            observed_ssrcs.lock().unwrap().iter().copied().collect();
                        let Ok(map) = buffer.map_readable() else {
                            return gst::PadProbeReturn::Ok;
                        };
                        let existing = map.as_slice();
                        // The sender SSRC of the report already in this packet is this session's
                        // own, which is what the REMB should be attributed to.
                        if existing.len() < 8 {
                            return gst::PadProbeReturn::Ok;
                        }
                        let sender_ssrc = u32::from_be_bytes([
                            existing[4],
                            existing[5],
                            existing[6],
                            existing[7],
                        ]);
                        let remb = build_remb(sender_ssrc, &ssrcs, kbps * 1000);
                        // Logged once on the first RTCP we get to piggyback on. Absence of this
                        // line means rtpbin is emitting no RTCP at all, which is a different
                        // problem from the SFU ignoring the estimate.
                        if !logged.swap(true, Ordering::AcqRel) {
                            info!(
                                "[remb] first REMB appended to {}-byte RTCP: \
                                 sender_ssrc={sender_ssrc:#010x} at {kbps} kbps, \
                                 reporting {} ssrc(s) {:?}",
                                existing.len(),
                                ssrcs.len(),
                                ssrcs
                                    .iter()
                                    .map(|s| format!("{s:#010x}"))
                                    .collect::<Vec<_>>()
                            );
                        }
                        let mut combined = Vec::with_capacity(existing.len() + remb.len());
                        combined.extend_from_slice(existing);
                        combined.extend_from_slice(&remb);
                        drop(map);

                        let mut out = gst::Buffer::from_mut_slice(combined);
                        // Carry timestamps over: rtpbin's downstream scheduling relies on them.
                        {
                            let out_ref = out.make_mut();
                            out_ref.set_pts(buffer.pts());
                            out_ref.set_dts(buffer.dts());
                        }
                        probe_info.data = Some(gst::PadProbeData::Buffer(out));
                        gst::PadProbeReturn::Ok
                    });
                });
            }

            // drop-on-latency was previously set to true as a workaround for an audio-stall
            // bug in rtpjitterbuffer after long mute + packet loss inside the first second.
            // With --bridge-video that setting actively destroys video: WebRTC sources on
            // this pipeline regularly exhibit 1–2 s RTP clock skew (visible in logs as
            // "delta - skew … reset skew"), and drop-on-latency=true then discards the
            // RTP fragments that belong to P-frames during those skew excursions —
            // producing classic decode-corruption (smeared macroblocks, ghost trails,
            // edge noise) or, when an IDR's fragments are hit, a stream where the
            // subscriber never gets a decode entry point and sees audio-only output.
            // Browsers don't drop in this scenario; they delay. We now mirror that.
            // If the original audio-stall bug resurfaces, the proper fix is a per-session
            // setting (drop-on-latency on the audio jitterbuffer only) rather than the
            // global rtpbin property.
            info!(
                "setting rtpbin latency to {latency} ms, drop-on-latency=false, \
                 do-retransmission=false"
            );
            elem.set_property_from_str("latency", &latency.to_string());
            elem.set_property_from_str("drop-on-latency", "false");
            // RTP retransmission (NACK/RTX, RFC 4588) is DISABLED on this path.
            //
            // It sounds right — browsers do it and it recovers loss — but the WHEP
            // answer from the manager strips a=ssrc-group:FID (it's a shared code
            // path that browsers depend on), so webrtcbin can never pair the RTX
            // repair SSRC with the main video. The [rtx-diag] capture proved the
            // consequence: rtx-success-count stays 0 for the whole session while
            // rtx-count climbs into the hundreds of thousands, a third (phantom)
            // jitterbuffer appears for the unpaired RTX SSRC, and the main stream
            // ends with more packets declared lost than pushed. That is a NACK-storm
            // feedback collapse, not real loss — on this same-DC path genuine loss is
            // ~0 until the storm congests the link. Turning retransmission off breaks
            // the cascade at the source: no NACKs → SMB sends no RTX → no phantom
            // jitterbuffer → clean sequence tracking. Keyframe recovery still comes
            // from the periodic/­on-subscribe PLI below. If FID is ever delivered to
            // this consumer specifically, revisit (RTX would then actually work).
            elem.set_property_from_str("do-retransmission", "false");
            // Note: do-lost=true was tried but mpegtsmux floods the log with "GAP event
            // outside segment, dropping" warnings — it doesn't know how to handle the
            // GstRTPPacketLost → GstEventGap propagation. The events are useful for raw
            // decoders that can error-conceal, but mpegtsmux is our downstream and it
            // just discards them. Leaving do-lost at its default (false).

            // Diagnostic instrumentation (read-only), gated on WHEP_SRT_RTP_DIAG: track every
            // jitterbuffer rtpbin creates and periodically dump its stats. The per-SSRC numbers
            // are what tell you whether loss is real on this path (num-lost) and whether RTX
            // recovers anything (rtx-count vs rtx-success-count).
            //
            // A caution when reading them, learned the hard way: against SMB the extra
            // jitterbuffer beyond audio + video is NOT an unpaired RTX stream, as was first
            // assumed. It is SMB's per-session bandwidth-probing stream — padding-only, on the
            // media payload type. And num-lost on the bridged video runs to a large fraction of
            // num-pushed while the picture decodes cleanly, so treat it as a sequence-jump
            // bookkeeping artifact of ssrc-rewrite egress rather than as real loss.
            let jbs: Arc<Mutex<Vec<(u32, u32, gst::Element)>>> =
                Arc::new(Mutex::new(Vec::new()));
            let jbs_for_signal = Arc::clone(&jbs);
            elem.connect("new-jitterbuffer", false, move |values| {
                let jb = values[1].get::<gst::Element>().ok()?;
                let session = values[2].get::<u32>().unwrap_or(0);
                let ssrc = values[3].get::<u32>().unwrap_or(0);
                // Enforce no-retransmission per jitterbuffer as well as on the
                // parent rtpbin: webrtcbin can re-assert do-retransmission when it
                // sets up a session, so we pin it off on each jitterbuffer the
                // moment it's created — before it can emit its first NACK.
                jb.set_property_from_str("do-retransmission", "false");
                let count = {
                    let mut v = jbs_for_signal.lock().unwrap();
                    v.push((session, ssrc, jb));
                    v.len()
                };
                if rtp_diag {
                    info!(
                        "[rtx-diag] new jitterbuffer: session={session} ssrc={ssrc:#010x} \
                         (total jitterbuffers now {count})"
                    );
                }
                None
            });
            if rtp_diag {
                let jbs_for_thread = Arc::clone(&jbs);
                std::thread::spawn(move || loop {
                    std::thread::sleep(Duration::from_secs(5));
                    let snapshot = {
                        let v = jbs_for_thread.lock().unwrap();
                        v.clone()
                    };
                    for (session, ssrc, jb) in &snapshot {
                        let stats = jb.property::<gst::Structure>("stats");
                        let g = |k: &str| stats.get::<u64>(k).unwrap_or(0);
                        info!(
                            "[rtx-diag] session={session} ssrc={ssrc:#010x} pushed={} \
                             lost={} late={} dup={} rtx-req={} rtx-ok={}",
                            g("num-pushed"),
                            g("num-lost"),
                            g("num-late"),
                            g("num-duplicates"),
                            g("rtx-count"),
                            g("rtx-success-count")
                        );
                    }
                });
            }
        }

        if elem_type == "GstWebRTCBin" {
            elem.connect_pad_added(move |elem, pad| {
                info!("webrtcbin pad added: '{}'", pad.name());

                /*
                   Note: When receiving multiple audio tracks (ssrcs), the first track is automatically exposed 'out' of the whepclientsrc bin
                   Other tracks are _not_ automatically exposed, so we have to handle that manually. That is why we listen for pad_added on webrtcbin and
                   then ghostpad our way out of the bins.
                */

                let caps = pad
                    .current_caps()
                    .unwrap_or_else(|| panic!("could not get current_caps on pad {}", pad.name()));
                let s = caps
                    .structure(0)
                    .expect("could not get structure 0 on caps");

                //info!("full structure: {:#?}", s);

                let media_type = s
                    .get::<String>("media")
                    .expect("could not get media from caps structure");

                if !pad.is_linked() {
                    //this is not automatically linked, we have to handle it. 
                    info!("pad '{}' is not automatically linked, handling ghostpads. media_type: {media_type}", pad.name());

                    let parent = elem.parent().expect("could not get webrtcbin parent");
                    let parent = parent
                        .dynamic_cast_ref::<gst::Bin>()
                        .expect("could not cast webrtcbin parent");

                    let new_pad_name = format!("{}_{}", media_type, pad.name());

                    let ghostpad = GhostPad::builder(PadDirection::Src)
                        .with_target(pad)
                        .expect("could not create ghostpad")
                        .name(&new_pad_name)
                        .build();
                    parent
                        .add_pad(&ghostpad)
                        .expect("could not add ghostpad to parent");

                    let parent_parent = parent.parent().expect("could not get parent parent");
                    if let Some(_pipe) = parent_parent.dynamic_cast_ref::<gst::Pipeline>() {
                        //info!("found pipeline.. no more ghostpads needed");
                    } else {
                        let parent_parent = parent_parent
                            .dynamic_cast_ref::<gst::Bin>()
                            .expect("could cast webrtcbin parent parent");

                        let ghostpad2 = GhostPad::builder(PadDirection::Src)
                            .with_target(&ghostpad)
                            .expect("could not create ghostpad2 with target ghostpad")
                            .name(&new_pad_name)
                            .build();
                        parent_parent
                            .add_pad(&ghostpad2)
                            .expect("could not add ghostpad2");
                    }
                }
            });
        }
    });

    // Make the "preferred pad never delivered" case loud instead of silent: if video bridging is
    // on and nothing has been bridged by the deadline, say so and name the candidates. This does
    // not hot-swap to a discarded pad — relinking off a fakesink needs a deliberate pad-block
    // dance — but it turns an unexplained black output into a one-line diagnosis.
    if bridge_video {
        let video_bridged_watchdog = video_bridged.clone();
        let video_pad_names_watchdog = video_pad_names.clone();
        std::thread::spawn(move || {
            std::thread::sleep(VIDEO_SELECT_FALLBACK);
            if !video_bridged_watchdog.load(Ordering::Acquire) {
                let candidates = video_pad_names_watchdog.lock().unwrap();
                if candidates.is_empty() {
                    warn!(
                        "no video track bridged after {}s: the source has not exposed any video \
                         pad yet",
                        VIDEO_SELECT_FALLBACK.as_secs()
                    );
                } else {
                    warn!(
                        "no video track bridged after {}s despite confirmed video pads {:?} — \
                         SRT output has no video",
                        VIDEO_SELECT_FALLBACK.as_secs(),
                        candidates
                    );
                }
            }
        });
    }

    // Per-SSRC RTP tally, logged every 5s as [pt-diag]. Opt-in via WHEP_SRT_RTP_DIAG=1: it is
    // how the padding-only probing stream was identified in the first place, so it is worth
    // keeping, but it prints a line per SSRC per interval and has no place in a normal run.
    //
    // Covers every pad the WHEP source exposes, including video pads that lose selection, since
    // the interesting question is usually about the track that was NOT bridged.
    struct PtTally {
        pad: String,
        packets: u64,
        markers: u64,
        padding: u64,
        first_seq: u16,
        last_seq: u16,
        // Payload shape names the stream: an H.264 media packet starts with a NAL header (low 5
        // bits = NAL type, and 0x78 is a STAP-A aggregate whose first inner NAL follows a 2-byte
        // size), whereas the probing stream's payload is zeros. Sampled from the first packet.
        sample_hex: String,
        min_payload: usize,
        max_payload: usize,
    }
    let pt_tally: Arc<Mutex<BTreeMap<(u32, u8), PtTally>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    if rtp_diag {
        let pt_tally_for_thread = Arc::clone(&pt_tally);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            let tally = pt_tally_for_thread.lock().unwrap();
            for ((ssrc, pt), t) in tally.iter() {
                info!(
                    "[pt-diag] pad='{}' ssrc={ssrc:#010x} pt={pt} packets={} markers={} \
                     padding={} seq={}..{} payload={}..{}B first_payload=[{}]",
                    t.pad,
                    t.packets,
                    t.markers,
                    t.padding,
                    t.first_seq,
                    t.last_seq,
                    t.min_payload,
                    t.max_payload,
                    t.sample_hex
                );
            }
        });
    }

    let video_preset_for_probe = video_preset.clone();
    let dump_frames_for_probe = dump_frames.clone();
    let video_pad_names_for_probe = video_pad_names.clone();
    let pt_tally_for_probe = Arc::clone(&pt_tally);
    let observed_ssrcs_for_probe = Arc::clone(&observed_ssrcs);
    input_whep_bin.connect_pad_added(move |elem, pad| {
        info!(
            "pad added on {} named '{}': '{}'",
            elem.type_(),
            elem.name(),
            pad.name()
        );

        let pipeline_clone = pipeline_clone.clone();
        let mixer_clone = mixer_clone.clone();
        let video_bridged = video_bridged.clone();
        let video_preset = video_preset_for_probe.clone();
        let dump_frames = dump_frames_for_probe.clone();
        let video_pad_names = video_pad_names_for_probe.clone();

        // [pt-diag] tally probe. Registered before the selection probe below and never removed,
        // so it keeps counting on pads that lose selection too — which is the whole point.
        let pt_tally = Arc::clone(&pt_tally_for_probe);
        let pt_tally_pad = pad.name().to_string();
        let observed_ssrcs = Arc::clone(&observed_ssrcs_for_probe);
        pad.add_probe(PadProbeType::BUFFER, move |_pad, probe_info| {
            if let Some(gst::PadProbeData::Buffer(ref buffer)) = probe_info.data {
                if let Ok(map) = buffer.map_readable() {
                    let bytes = map.as_slice();
                    if bytes.len() >= 12 {
                        // Recorded unconditionally: REMB needs these even with diagnostics off.
                        observed_ssrcs.lock().unwrap().insert(u32::from_be_bytes([
                            bytes[8], bytes[9], bytes[10], bytes[11],
                        ]));
                    }
                }
            }
            if !rtp_diag {
                return gst::PadProbeReturn::Ok;
            }
            if let Some(gst::PadProbeData::Buffer(ref buffer)) = probe_info.data {
                if let Ok(map) = buffer.map_readable() {
                    let bytes = map.as_slice();
                    if bytes.len() >= 12 {
                        let payload_type = bytes[1] & 0x7f;
                        let marker = bytes[1] & 0x80 != 0;
                        let has_padding = bytes[0] & 0x20 != 0;
                        let seq = u16::from_be_bytes([bytes[2], bytes[3]]);
                        let ssrc =
                            u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);

                        let payload_len = rtp_payload_len(bytes).unwrap_or(0);
                        let payload = bytes.get(bytes.len() - payload_len..).unwrap_or(&[]);

                        let mut tally = pt_tally.lock().unwrap();
                        let entry =
                            tally.entry((ssrc, payload_type)).or_insert_with(|| PtTally {
                                pad: pt_tally_pad.clone(),
                                packets: 0,
                                markers: 0,
                                padding: 0,
                                first_seq: seq,
                                last_seq: seq,
                                sample_hex: payload
                                    .iter()
                                    .take(8)
                                    .map(|b| format!("{b:02x}"))
                                    .collect::<Vec<_>>()
                                    .join(" "),
                                min_payload: payload_len,
                                max_payload: payload_len,
                            });
                        entry.packets += 1;
                        if marker {
                            entry.markers += 1;
                        }
                        if has_padding {
                            entry.padding += 1;
                        }
                        entry.min_payload = entry.min_payload.min(payload_len);
                        entry.max_payload = entry.max_payload.max(payload_len);
                        entry.last_seq = seq;
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });

        let probe_logged = AtomicBool::new(false);
        pad.add_probe(PadProbeType::BUFFER, move |pad, probe_info| {
            // Does this packet prove the stream carries real media rather than being SMB's
            // bandwidth-probing stream? See rtp_carries_media.
            let media_evidence = match probe_info.data {
                Some(gst::PadProbeData::Buffer(ref buffer)) => buffer
                    .map_readable()
                    .ok()
                    .map(|map| rtp_carries_media(map.as_slice()))
                    .unwrap_or(false),
                _ => false,
            };

            let Some(caps) = pad.current_caps() else {
                error!("buffer probe: could not get caps from pad '{}'", pad.name());
                return gstreamer::PadProbeReturn::Remove;
            };
            let Some(structure) = caps.structure(0) else {
                error!("buffer probe: caps on pad '{}' have no structure", pad.name());
                return gstreamer::PadProbeReturn::Remove;
            };
            let Ok(media_type) = structure.get::<String>("media") else {
                error!("buffer probe: caps on pad '{}' have no media field", pad.name());
                return gstreamer::PadProbeReturn::Remove;
            };

            // Include the pad name: with more than one video pad the bridge winner is decided
            // by whichever pad first proves it carries media, so without the name the logs can't
            // tell you which track was actually bridged versus discarded. Logged once per pad —
            // the probe below re-runs for every buffer while a track is still unproven.
            if !probe_logged.swap(true, Ordering::AcqRel) {
                info!(
                    "getting {media_type} track on pad '{}' (caps: {})",
                    pad.name(),
                    structure
                );
            }
            match media_type.as_str() {
                "audio" => {
                    let pipe_bin = pipeline_clone
                        .dynamic_cast_ref::<gst::Bin>()
                        .expect("could not cast pipeline to bin");

                    let decodebin = ElementFactory::make("decodebin")
                        .build()
                        .expect("could not create decodebin");
                    pipe_bin
                        .add(&decodebin)
                        .expect("could not add decodebin to pipe_bin");
                    decodebin
                        .sync_state_with_parent()
                        .expect("could not sync_state on decode_bin");

                    let pipe_bin_clone = pipe_bin.clone();

                    let mixer_clone = mixer_clone.clone();
                    decodebin.connect_pad_added(move |elem, pad| {
                        info!("pad '{}' added on decodebin '{}'", pad.name(), elem.name());

                        let audioconvert = ElementFactory::make("audioconvert")
                            .build()
                            .expect("could not create audioconvert");
                        let audioresample = ElementFactory::make("audioresample")
                            .build()
                            .expect("could not create audioresample");
                        let caps = ElementFactory::make("capsfilter")
                            .build()
                            .expect("could not create capsfiler");
                        caps.set_property_from_str("caps", "audio/x-raw,format=F32LE,rate=48000");

                        let elements = [&audioconvert, &audioresample, &caps];

                        pipe_bin_clone
                            .add_many(elements)
                            .expect("could not add_many");
                        for elem in elements {
                            elem.sync_state_with_parent()
                                .expect("could not sync_state_with_parent");
                        }

                        gst::Element::link_many(elements).expect("could not link many on elements");

                        //-- setup links from decodebin leg to audiomixer --
                        let caps_src_pad = caps.static_pad("src").unwrap();

                        let mixer_input_pad = mixer_clone
                            .request_pad_simple("sink_%u")
                            .expect("could not get audio mixer input pad");

                        caps_src_pad
                            .link(&mixer_input_pad)
                            .expect("could not link input audio to audiomixer");

                        //link decodebin pad to audioconvert
                        pad.link(&audioconvert.static_pad("sink").unwrap())
                            .expect("could not link decodebin to audioconvert sink");
                    });

                    let decodebin_pad = decodebin
                        .sink_pads()
                        .into_iter()
                        .next()
                        .expect("audio decodebin has no sink pad");
                    pad.link(&decodebin_pad)
                        .expect("could not link from webrtcbin audio pad to decodebin");
                }
                "video" => {
                    // Record this pad as a confirmed video track. It has to happen here rather
                    // than at pad-added because pad *names* do not identify media type: the
                    // automatically-exposed pad is called `video_0` even when it carries the OPUS
                    // audio track, so only the caps on the first buffer settle what a pad really
                    // is. That is also why selection below cannot prefer a particular pad name.
                    {
                        let mut names = video_pad_names.lock().unwrap();
                        names.insert(pad.name().to_string());
                        if names.len() > 1 {
                            info!("confirmed video pads so far: {:?}", names);
                        }
                    }

                    // Selection among several video tracks, in priority order.
                    //
                    // SMB gives a WHEP consumer more than one video track on the same `a-mid` and
                    // the same payload type: the forwarded publisher, plus one padding-only stream
                    // per session that exists for bandwidth probing. Nothing in the caps separates
                    // them, and pad *names* carry no identity either — which pad number the real
                    // video lands on varies per session (measured: `video_src_1` in some runs,
                    // `video_src_2` in others), so preferring a name is unsound.
                    //
                    // What does separate them is the payload. A padding-only packet has zero media
                    // bytes once RFC 3550 padding is subtracted, and the probing stream is
                    // padding-only for its entire life (measured: padding on 100% of packets, zero
                    // marker bits, sequence starting at 0), whereas real video carries media bytes
                    // and frame markers. So skip a track whose first packet is pure padding —
                    // crucially WITHOUT consuming the single bridge slot, so the real video still
                    // wins it whenever it shows up. Before this check, selection was
                    // first-buffer-wins between the two and would intermittently bridge the
                    // padding stream, which `decodebin` can never negotiate caps for: the SRT
                    // output then came out audio-only.
                    //
                    // Until a track proves it carries media, drop its buffers and decide nothing.
                    // Dropping is what makes waiting safe: the buffer never reaches the still-
                    // unlinked pad, so there is no GST_FLOW_NOT_LINKED and no bus error, and the
                    // single bridge slot stays free for whichever track proves itself first. A
                    // probing track simply never gets here, so it never wins and never gets
                    // linked; the real video claims the slot on its first packet.
                    if bridge_video && !media_evidence {
                        return gstreamer::PadProbeReturn::Drop;
                    }

                    let discard_reason = if !bridge_video {
                        Some("video bridging disabled")
                    } else if video_bridged.swap(true, Ordering::AcqRel) {
                        Some("another video track was already bridged")
                    } else {
                        None
                    };

                    if let Some(reason) = discard_reason {
                        info!(
                            "discarding video pad '{}' to fakesink ({reason})",
                            pad.name()
                        );
                        let pipe_bin = pipeline_clone
                            .dynamic_cast_ref::<gst::Bin>()
                            .expect("could not cast pipeline to bin");
                        let fakesink = ElementFactory::make("fakesink")
                            .build()
                            .expect("could not create video fakesink");
                        pipe_bin
                            .add(&fakesink)
                            .expect("could not add video fakesink to pipeline");
                        fakesink
                            .sync_state_with_parent()
                            .expect("could not sync state on fakesink");
                        pad.link(&fakesink.static_pad("sink").unwrap())
                            .expect("could not link video pad to fakesink");
                    } else {
                        let encoding_name = structure
                            .get::<String>("encoding-name")
                            .unwrap_or_default();

                        let pipe_bin = pipeline_clone
                            .dynamic_cast_ref::<gst::Bin>()
                            .expect("could not cast pipeline to bin");

                        if passthrough && encoding_name.eq_ignore_ascii_case("H264") {
                            info!(
                                "bridging H.264 video track on pad '{}' to SRT output (passthrough — no transcode)",
                                pad.name()
                            );

                            // rtph264depay → capsfilter(stream-format=avc) → h264parse →
                            // capsfilter(byte-stream) → queue → mpegtsmux.
                            // Forcing stream-format=avc on depay's src caps makes it populate
                            // codec_data with the SDP sprop-parameter-sets; h264parse with
                            // config-interval=-1 then injects those SPS/PPS inline ahead of
                            // every IDR when converting to byte-stream — without this the
                            // depay drops sprop-parameter-sets and most NALs come through as
                            // "short NAL" warnings.
                            let depay = ElementFactory::make("rtph264depay")
                                .build()
                                .expect("could not create rtph264depay");
                            let depay_avc_caps = ElementFactory::make("capsfilter")
                                .build()
                                .expect("could not create depay AVC capsfilter");
                            depay_avc_caps.set_property_from_str(
                                "caps",
                                "video/x-h264,stream-format=avc,alignment=au",
                            );
                            let h264parse = ElementFactory::make("h264parse")
                                .build()
                                .expect("could not create h264parse");
                            h264parse.set_property_from_str("config-interval", "-1");
                            // Force h264parse to re-parse instead of passing buffers through
                            // unchanged: with the default disable-passthrough=false, h264parse
                            // can slip into passthrough mode when input alignment matches the
                            // downstream request, leaving NAL-aligned (rather than AU-aligned)
                            // buffers heading into mpegtsmux which then mis-frames the stream.
                            // The format change (avc → byte-stream) already forces re-parsing
                            // in practice today, but disable-passthrough=true is cheap insurance
                            // against a build that interprets the negotiation differently.
                            h264parse.set_property_from_str("disable-passthrough", "true");
                            let stream_caps = ElementFactory::make("capsfilter")
                                .build()
                                .expect("could not create h264 stream capsfilter");
                            stream_caps.set_property_from_str(
                                "caps",
                                "video/x-h264,stream-format=byte-stream,alignment=au",
                            );
                            let queue = ElementFactory::make("queue")
                                .name("video_queue")
                                .build()
                                .expect("could not create video queue");

                            let elements = [&depay, &depay_avc_caps, &h264parse, &stream_caps, &queue];
                            pipe_bin
                                .add_many(elements)
                                .expect("could not add H.264 passthrough elements to pipeline");
                            for elem in elements {
                                elem.sync_state_with_parent()
                                    .expect("could not sync H.264 passthrough element state");
                            }
                            gst::Element::link_many(elements)
                                .expect("could not link H.264 passthrough chain");

                            // The WHEP rtpjitterbuffer periodically resets its skew estimation
                            // (visible in logs as "delta - skew … too big, reset skew") and the
                            // immediately-surrounding buffers come out of rtph264depay with
                            // PTS=NONE.  mpegtsmux then drops them with "Buffer has no timestamp"
                            // — which on the wire shows up as dropped P-frames and visible
                            // blockiness even when the source bitrate is fine.  Stamp those
                            // buffers with the previous buffer's PTS plus a 1 ms tick — that
                            // keeps timestamps monotonic and local to the surrounding sequence
                            // (using wall-clock running time produced non-monotonic DTS that
                            // ffmpeg then had to "replace by guess").  Constrained Baseline has
                            // no B-frames so DTS == PTS.
                            let stamper_target = queue.clone();
                            let last_pts: Mutex<Option<gst::ClockTime>> = Mutex::new(None);
                            let stamper_warned = AtomicBool::new(false);
                            h264parse
                                .static_pad("src")
                                .unwrap()
                                .add_probe(PadProbeType::BUFFER, move |_pad, info| {
                                    if let Some(gst::PadProbeData::Buffer(ref mut buf)) = info.data
                                    {
                                        let mut last = last_pts.lock().unwrap();
                                        match buf.pts() {
                                            Some(pts) => *last = Some(pts),
                                            None => {
                                                let stamp = last
                                                    .map(|p| p + gst::ClockTime::from_mseconds(1))
                                                    .or_else(|| {
                                                        stamper_target.current_running_time()
                                                    });
                                                if let Some(stamp) = stamp {
                                                    let buf_mut = buf.make_mut();
                                                    buf_mut.set_pts(Some(stamp));
                                                    if buf_mut.dts().is_none() {
                                                        buf_mut.set_dts(Some(stamp));
                                                    }
                                                    *last = Some(stamp);
                                                    if !stamper_warned
                                                        .swap(true, Ordering::Relaxed)
                                                    {
                                                        info!(
                                                            "video chain: stamping PTS=NONE buffers (jitterbuffer skew artefact)"
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    gst::PadProbeReturn::Ok
                                });

                            let mux = pipeline_clone
                                .by_name("mux")
                                .expect("could not find mpegtsmux");
                            let h264_caps = gst::Caps::builder("video/x-h264").build();
                            let mux_video_pad = mux
                                .pad_template_list()
                                .into_iter()
                                .find(|t| {
                                    t.direction() == gst::PadDirection::Sink
                                        && t.presence() == gst::PadPresence::Request
                                        && t.caps().can_intersect(&h264_caps)
                                })
                                .inspect(|t| {
                                    info!("requesting mpegtsmux video pad via template '{}'", t.name_template());
                                })
                                .and_then(|t| mux.request_pad(&t, None, None))
                                .expect("could not request a video pad from mpegtsmux");
                            queue
                                .static_pad("src")
                                .unwrap()
                                .link(&mux_video_pad)
                                .expect("could not link video queue src to mpegtsmux");

                            pad.link(&depay.static_pad("sink").unwrap())
                                .expect("could not link video whep pad to rtph264depay");
                        } else {
                            info!(
                                "bridging {encoding_name} video track on pad '{}' to SRT output (transcode to H.264)",
                                pad.name()
                            );

                            let decodebin = ElementFactory::make("decodebin")
                                .build()
                                .expect("could not create video decodebin");
                            pipe_bin
                                .add(&decodebin)
                                .expect("could not add video decodebin to pipeline");
                            decodebin
                                .sync_state_with_parent()
                                .expect("could not sync video decodebin state");

                            // Clone references for the decodebin pad-added closure.
                            // pipe_bin borrow must end before pipeline_clone.clone() — NLL handles this.
                            let pipe_bin_clone = pipe_bin.clone();
                            let pipeline_for_mux = pipeline_clone.clone();
                            let video_preset = video_preset.clone();
                            let dump_frames = dump_frames.clone();

                            decodebin.connect_pad_added(move |_elem, src_pad| {
                                info!("video decodebin src pad added: '{}'", src_pad.name());

                                // DIAGNOSTIC frame dump: write the DECODED frames (before any
                                // re-encode) as JPEG. A smeary dumped frame => the source/decode
                                // is bad (not the transcode); a crisp one => corruption is
                                // downstream. Rate-limited to 2 fps. Enable via WHEP_SRT_DUMP_FRAMES=/dir.
                                if let Some(ref dir) = dump_frames {
                                    let q = ElementFactory::make("queue")
                                        .build()
                                        .expect("could not create dump queue");
                                    let vconv = ElementFactory::make("videoconvert")
                                        .build()
                                        .expect("could not create dump videoconvert");
                                    let vrate = ElementFactory::make("videorate")
                                        .build()
                                        .expect("could not create dump videorate");
                                    let ratecaps = ElementFactory::make("capsfilter")
                                        .build()
                                        .expect("could not create dump ratecaps");
                                    ratecaps.set_property_from_str("caps", "video/x-raw,framerate=2/1");
                                    let jpegenc = ElementFactory::make("jpegenc")
                                        .build()
                                        .expect("could not create dump jpegenc");
                                    let sink = ElementFactory::make("multifilesink")
                                        .build()
                                        .expect("could not create dump multifilesink");
                                    sink.set_property_from_str(
                                        "location",
                                        &format!("{dir}/frame-%05d.jpg"),
                                    );
                                    let elements = [&q, &vconv, &vrate, &ratecaps, &jpegenc, &sink];
                                    pipe_bin_clone
                                        .add_many(elements)
                                        .expect("could not add dump elements");
                                    for e in elements {
                                        e.sync_state_with_parent()
                                            .expect("could not sync dump element");
                                    }
                                    gst::Element::link_many(elements)
                                        .expect("could not link dump chain");
                                    src_pad
                                        .link(&q.static_pad("sink").unwrap())
                                        .expect("could not link decoded src to dump queue");
                                    info!("[frame-dump] writing decoded frames to {dir}/frame-*.jpg (2 fps)");
                                    return;
                                }

                                // Decode → convert → encode H.264 → byte-stream caps → queue → mux.
                                // tune=zerolatency: no B-frames / no lookahead — required for live.
                                // bframes=0 and CABAC are already correct via tune=zerolatency.
                                // speed-preset / bitrate / key-int-max are tunable via CLI flags;
                                // see Args for env var names and defaults.
                                let videoconvert = ElementFactory::make("videoconvert")
                                    .build()
                                    .expect("could not create videoconvert");
                                let x264enc = ElementFactory::make("x264enc")
                                    .build()
                                    .expect("could not create x264enc — ensure gstreamer1.0-plugins-ugly is installed");
                                x264enc.set_property_from_str("tune", "zerolatency");
                                x264enc.set_property_from_str("speed-preset", &video_preset);
                                x264enc.set_property_from_str("bitrate", &video_bitrate.to_string());
                                x264enc.set_property_from_str("key-int-max", &video_key_int.to_string());
                                let h264parse = ElementFactory::make("h264parse")
                                    .build()
                                    .expect("could not create h264parse");
                                h264parse.set_property_from_str("config-interval", "-1");
                                let stream_caps = ElementFactory::make("capsfilter")
                                    .build()
                                    .expect("could not create h264 stream capsfilter");
                                stream_caps.set_property_from_str(
                                    "caps",
                                    "video/x-h264,stream-format=byte-stream,alignment=au",
                                );
                                let queue = ElementFactory::make("queue")
                                    .name("video_queue")
                                    .build()
                                    .expect("could not create video queue");

                                let elements = [&videoconvert, &x264enc, &h264parse, &stream_caps, &queue];
                                pipe_bin_clone
                                    .add_many(elements)
                                    .expect("could not add video encode elements to pipeline");
                                for elem in elements {
                                    elem.sync_state_with_parent()
                                        .expect("could not sync video encode element state");
                                }
                                gst::Element::link_many(elements)
                                    .expect("could not link video encode chain");

                                let mux = pipeline_for_mux
                                    .by_name("mux")
                                    .expect("could not find mpegtsmux");
                                let h264_caps = gst::Caps::builder("video/x-h264").build();
                                let mux_video_pad = mux
                                    .pad_template_list()
                                    .into_iter()
                                    .find(|t| {
                                        t.direction() == gst::PadDirection::Sink
                                            && t.presence() == gst::PadPresence::Request
                                            && t.caps().can_intersect(&h264_caps)
                                    })
                                    .inspect(|t| {
                                        info!("requesting mpegtsmux video pad via template '{}'", t.name_template());
                                    })
                                    .and_then(|t| mux.request_pad(&t, None, None))
                                    .expect("could not request a video pad from mpegtsmux");
                                queue
                                    .static_pad("src")
                                    .unwrap()
                                    .link(&mux_video_pad)
                                    .expect("could not link video queue src to mpegtsmux");

                                src_pad
                                    .link(&videoconvert.static_pad("sink").unwrap())
                                    .expect("could not link video decodebin src to videoconvert");
                            });

                            let decodebin_sink = decodebin
                                .sink_pads()
                                .into_iter()
                                .next()
                                .expect("video decodebin has no sink pad");
                            pad.link(&decodebin_sink)
                                .expect("could not link video whep pad to video decodebin");
                        }
                    }
                }
                _ => {
                    error!("unhandled media type");
                }
            }

            gstreamer::PadProbeReturn::Remove
        });
    });

    // Start pipeline - ICE role is configured via webrtcbin-ready signal
    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");

    let pipeline_clone = pipeline.clone();

    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::StateChanged(state) => {
                if !state
                    .src()
                    .unwrap()
                    .type_()
                    .to_string()
                    .contains("GstPipeline")
                {
                    continue;
                }

                log::debug!(
                    "pipeline change: {:?} -> {:?}",
                    state.old(),
                    state.current()
                );

                if dot_debug {
                    let pipe_bin = pipeline_clone.dynamic_cast_ref::<gst::Bin>().unwrap();
                    debug_pipeline(pipe_bin, &format!("{:?}", state.current()));
                }
            }
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                error!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );

                if dot_debug {
                    let pipe_bin = pipeline_clone.dynamic_cast_ref::<gst::Bin>().unwrap();
                    debug_pipeline(pipe_bin, "error");
                }

                break;
            }
            _ => (),
        }
    }

    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");

    std::thread::sleep(std::time::Duration::from_secs(1));
}

// Patches MPEG-TS buffers in-place to remove the HDMV registration descriptor from the PMT.
// mpegtsmux adds this descriptor unconditionally for H.264 streams.  When present without an
// accompanying AVCDecoderConfigurationRecord, FFmpeg >= 8 enters HDMV mode and refuses to
// read inline SPS/PPS, causing "non-existing PPS 0 referenced" even when SPS/PPS are present
// in the elementary stream.  Removing the HDMV identifier makes FFmpeg treat the stream as
// plain TS H.264 and parse inline SPS/PPS normally.
struct PmtPatcher {
    pmt_pid: Option<u16>,
    patch_logged: bool,
}

impl PmtPatcher {
    fn new() -> Self {
        Self {
            pmt_pid: None,
            patch_logged: false,
        }
    }

    fn process(&mut self, data: &mut [u8]) {
        let mut pos = 0;
        while pos + 188 <= data.len() {
            let pkt = &mut data[pos..pos + 188];
            if pkt[0] != 0x47 {
                pos += 1;
                continue;
            }
            let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
            let pusi = pkt[1] & 0x40 != 0;
            // adaptation_field_control bits [5:4] of byte 3
            let afc = (pkt[3] >> 4) & 0x03;
            let payload_start: Option<usize> = match afc {
                1 => Some(4),
                3 => {
                    let af_len = pkt[4] as usize;
                    let s = 5 + af_len;
                    if s < 188 { Some(s) } else { None }
                }
                _ => None,
            };
            if let Some(ps) = payload_start {
                if pid == 0 && pusi {
                    self.read_pat(&pkt[ps..]);
                } else if self.pmt_pid == Some(pid) && pusi {
                    self.patch_pmt(&mut pkt[ps..]);
                }
            }
            pos += 188;
        }
    }

    fn read_pat(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let ptr = payload[0] as usize;
        if 1 + ptr + 12 > payload.len() {
            return;
        }
        let s = &payload[1 + ptr..];
        if s[0] != 0x00 {
            return; // table_id must be PAT (0x00)
        }
        let sec_len = ((s[1] as usize & 0x0F) << 8) | s[2] as usize;
        let sec_total = 3 + sec_len;
        if sec_total > s.len() {
            return;
        }
        let entries_end = sec_total - 4; // exclude 4-byte CRC32 at end
        let mut i = 8usize; // program entries start after 8-byte fixed header
        while i + 4 <= entries_end {
            let prog = ((s[i] as u16) << 8) | s[i + 1] as u16;
            let ppid = ((s[i + 2] as u16 & 0x1F) << 8) | s[i + 3] as u16;
            if prog != 0 {
                if self.pmt_pid.is_none() {
                    info!("[PMT-PATCH] PMT PID: {ppid:#05x}");
                    self.pmt_pid = Some(ppid);
                }
                return;
            }
            i += 4;
        }
    }

    fn patch_pmt(&mut self, payload: &mut [u8]) {
        if payload.is_empty() {
            return;
        }
        let ptr = payload[0] as usize;
        let so = 1 + ptr;
        if so + 12 > payload.len() {
            return;
        }
        let s = &mut payload[so..];
        if s[0] != 0x02 {
            return; // table_id must be PMT (0x02)
        }
        let sec_len = ((s[1] as usize & 0x0F) << 8) | s[2] as usize;
        let sec_total = 3 + sec_len;
        if sec_total > s.len() {
            return; // PMT spans packets; skip
        }
        let s = &mut s[..sec_total];

        // program_info_length at bytes [10..12] (upper 4 bits reserved)
        let pil = ((s[10] as usize & 0x0F) << 8) | s[11] as usize;
        let prog_end = 12 + pil; // end of program-level descriptor loop
        let crc_pos = sec_total - 4;
        if prog_end > crc_pos {
            return;
        }

        let mut patched = false;

        // Pass 1: scan the program-level descriptor loop.
        patched |= patch_hdmv_in_range(s, 12, prog_end);

        // Pass 2: scan each elementary stream's ES_info descriptor loop.
        // Each ES entry is: stream_type(1) + elementary_PID(2) + ES_info_length(2) + descriptors.
        let mut i = prog_end;
        while i + 5 <= crc_pos {
            let es_info_len = ((s[i + 3] as usize & 0x0F) << 8) | s[i + 4] as usize;
            let es_desc_start = i + 5;
            let es_desc_end = es_desc_start + es_info_len;
            if es_desc_end > crc_pos {
                break;
            }
            patched |= patch_hdmv_in_range(s, es_desc_start, es_desc_end);
            i = es_desc_end;
        }

        if patched {
            // Recompute MPEG-2 CRC32 over the section (table_id through last byte before CRC).
            let crc = crc32_mpeg2(&s[..crc_pos]);
            s[crc_pos] = (crc >> 24) as u8;
            s[crc_pos + 1] = (crc >> 16) as u8;
            s[crc_pos + 2] = (crc >> 8) as u8;
            s[crc_pos + 3] = crc as u8;
            if !self.patch_logged {
                info!("[PMT-PATCH] Removed HDMV registration descriptor from PMT");
                self.patch_logged = true;
            }
        }
    }
}

// Scans a PMT descriptor loop in `s[start..end]` for a registration_descriptor (tag 0x05)
// whose 4-byte format_identifier equals "HDMV", and zeroes that identifier.  Returns true if a
// patch was applied.  mpegtsmux can emit this descriptor either in the program_info loop or in
// the per-elementary-stream ES_info loop, depending on version, so the caller invokes this for
// both.
fn patch_hdmv_in_range(s: &mut [u8], start: usize, end: usize) -> bool {
    let mut i = start;
    while i + 2 <= end {
        let tag = s[i];
        let len = s[i + 1] as usize;
        if i + 2 + len > end {
            break;
        }
        if tag == 0x05 && len >= 4 && &s[i + 2..i + 6] == b"HDMV" {
            s[i + 2..i + 6].copy_from_slice(&[0u8; 4]);
            return true;
        }
        i += 2 + len;
    }
    false
}

// MPEG-2 CRC32: poly 0x04C11DB7, init 0xFFFFFFFF, no reflection, no final XOR.
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        for bit in (0..8u32).rev() {
            let b = u32::from((byte >> bit) & 1);
            let msb = crc >> 31;
            crc <<= 1;
            if msb ^ b != 0 {
                crc ^= 0x04C11DB7;
            }
        }
    }
    crc
}

fn debug_pipeline(pipe: &gst::Bin, str: &str) {
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

    let filename = format!("{:?}-{}", epoch.as_secs(), str);
    pipe.debug_to_dot_file(DebugGraphDetails::ALL, &filename);

    info!(
        "debugging to file: '{filename}.dot'. Use xdot application to view or convert to svg with 'dot -Tsvg {filename}.dot -o {filename}.svg'"
    );

    /*
    use std::{os::unix::process::CommandExt, process::Command};
    let _ = std::process::Command::new("dot")
        .arg("-Tsvg")
        .arg(format!("{filename}.dot"))
        .arg("-o")
        .arg(format!("{filename}.svg"))
        .spawn();
    */
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a REMB packet the way a receiver (SMB) would, so the tests assert on the wire format
    /// rather than on our own construction of it.
    fn parse_remb(p: &[u8]) -> (u32, u64, Vec<u32>) {
        assert_eq!(p[0], 0x8F, "V=2, FMT=15");
        assert_eq!(p[1], 0xCE, "PT=206 PSFB");
        let words = u16::from_be_bytes([p[2], p[3]]) as usize + 1;
        assert_eq!(words * 4, p.len(), "length field must match actual size");
        let sender = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
        assert_eq!(&p[8..12], &[0, 0, 0, 0], "media ssrc unused");
        assert_eq!(&p[12..16], b"REMB");
        let n = p[16] as usize;
        let exponent = (p[17] >> 2) as u32;
        let mantissa = (((p[17] & 0x03) as u32) << 16) | ((p[18] as u32) << 8) | p[19] as u32;
        let ssrcs = (0..n)
            .map(|i| {
                let o = 20 + i * 4;
                u32::from_be_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]])
            })
            .collect();
        (sender, (mantissa as u64) << exponent, ssrcs)
    }

    #[test]
    fn remb_round_trips_a_typical_bitrate() {
        let packet = build_remb(0xDEADBEEF, &[0x11223344], 10_000_000);
        let (sender, bitrate, ssrcs) = parse_remb(&packet);
        assert_eq!(sender, 0xDEADBEEF);
        assert_eq!(ssrcs, vec![0x11223344]);
        // Exponent scaling loses precision; 0.5% is far tighter than SMB cares about.
        let error = (bitrate as f64 - 10_000_000.0).abs() / 10_000_000.0;
        assert!(error < 0.005, "bitrate {bitrate} too far from 10 Mbps");
    }

    #[test]
    fn remb_reports_every_observed_ssrc() {
        let packet = build_remb(1, &[0xAAAA_AAAA, 0xBBBB_BBBB, 0xCCCC_CCCC], 4_000_000);
        let (_, _, ssrcs) = parse_remb(&packet);
        assert_eq!(ssrcs, vec![0xAAAA_AAAA, 0xBBBB_BBBB, 0xCCCC_CCCC]);
        assert_eq!(packet.len(), 20 + 3 * 4);
    }

    #[test]
    fn remb_handles_bitrates_that_need_no_exponent() {
        // Under 2^18 bps the mantissa holds the value outright, exponent 0.
        let packet = build_remb(7, &[9], 200_000);
        assert_eq!(packet[17] >> 2, 0, "exponent should be zero");
        let (_, bitrate, _) = parse_remb(&packet);
        assert_eq!(bitrate, 200_000, "small bitrates must be exact");
    }

    #[test]
    fn remb_survives_an_absurdly_large_bitrate() {
        // Must not panic or overflow the 18-bit mantissa; 1 Gbps is well past anything real.
        let packet = build_remb(1, &[2], 1_000_000_000);
        let (_, bitrate, _) = parse_remb(&packet);
        let error = (bitrate as f64 - 1e9).abs() / 1e9;
        assert!(error < 0.005, "bitrate {bitrate} too far from 1 Gbps");
    }
}
