use clap::Parser;
use env_logger::Env;
use log::{self, error, info, warn};
use std::collections::BTreeSet;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};
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

    /// Jitterbuffer latency in milliseconds (sets rtpbin latency and liveadder min-upstream-latency)
    #[clap(long, env = "WHEP_SRT_JITTERBUFFER_LATENCY", default_value_t = 200)]
    pub latency: u64,

    /// Authorization token for WHEP endpoint
    #[clap(long, env = "WHEP_SRT_AUTH_TOKEN")]
    pub auth_token: Option<String>,

    /// Bridge video tracks from WHEP to SRT output (re-encodes to H.264 in MPEG-TS)
    #[clap(long)]
    pub bridge_video: bool,

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

    /// Output video size as WIDTHxHEIGHT (used when --bridge-video is set).  The encoder runs from
    /// startup so that video is present in the very first PAT/PMT, which means the output size must
    /// be known before the WHEP source's caps are — hence a setting rather than a passthrough of
    /// whatever arrives.  Incoming video is scaled into this size with borders added rather than
    /// stretched, so the aspect ratio is preserved.
    #[clap(long, env = "WHEP_SRT_VIDEO_SIZE", default_value_t = String::from("1280x720"))]
    pub video_size: String,

    /// Output video framerate in frames per second (used when --bridge-video is set).  Fixed for
    /// the same reason as --video-size.
    #[clap(long, env = "WHEP_SRT_VIDEO_FPS", default_value_t = 25)]
    pub video_fps: u32,
}

/// Build the always-present video branch: a black fill into a compositor, then the H.264 encoder
/// and the mpegtsmux video pad.
///
/// Present from startup so the streamheader PAT/PMT declares video in the very first packets. The
/// real video pad arrives from WHEP seconds later; anything that reads the program map before then
/// — an SRT ingest the bridge dials on start, a relay that latches streamheaders and never
/// re-reads them, a hardware decoder — would otherwise see an audio-only program and never pick
/// video up, even though the PID starts carrying H.264 moments later. Seen in production as a
/// video PID sitting outside the program, typed "unknown codec".
fn build_video_branch(
    width: u32,
    height: u32,
    fps: u32,
    preset: &str,
    bitrate: u32,
    key_int: u32,
) -> String {
    format!(
        "videotestsrc name=videofill pattern=black is-live=true \
         ! video/x-raw,width={width},height={height},framerate={fps}/1 ! comp. \
         compositor name=comp background=black \
         ! video/x-raw,width={width},height={height},framerate={fps}/1 \
         ! videoconvert \
         ! x264enc tune=zerolatency speed-preset={preset} bitrate={bitrate} key-int-max={key_int} \
         ! h264parse config-interval=-1 \
         ! video/x-h264,stream-format=byte-stream,alignment=au ! queue ! mux. "
    )
}

/// Parse a `WIDTHxHEIGHT` size, falling back to 1280x720 on anything unparseable.
///
/// A bad value must not be fatal: these bridges run unattended in production, and refusing to
/// start would take audio down too — audio-only is a far better failure than no output at all.
fn parse_video_size(spec: &str) -> (u32, u32) {
    const FALLBACK: (u32, u32) = (1280, 720);
    let Some((w, h)) = spec.trim().split_once(['x', 'X']) else {
        warn!(
            "could not parse --video-size '{spec}', using {}x{}",
            FALLBACK.0, FALLBACK.1
        );
        return FALLBACK;
    };
    match (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
        // Odd dimensions break I420 chroma subsampling, so require even and non-zero.
        (Ok(w), Ok(h)) if w > 0 && h > 0 && w % 2 == 0 && h % 2 == 0 => (w, h),
        _ => {
            warn!(
                "invalid --video-size '{spec}', using {}x{}",
                FALLBACK.0, FALLBACK.1
            );
            FALLBACK
        }
    }
}

/// Whether an RTP packet is evidence that its stream carries real media, rather than being a
/// bandwidth-probing stream.
///
/// Some SFUs hand a WHEP consumer a padding-only stream alongside the real video, on the same
/// payload type and the same `a-mid`, so the caps cannot tell them apart. Measured against
/// Symphony Media Bridge, one such stream appears per session, and which pad number the real video
/// arrives on varies between sessions — so a track cannot be chosen by pad name either.
///
/// Two signals, either sufficient. An unpadded packet (P bit clear) is ordinary media: measured
/// over a full session the real video track set P on 0 of 4891 packets while the probing stream set
/// it on 293 of 293. A marker bit is the other tell — it terminates a video frame, and the probing
/// stream never sets one — which keeps this from misjudging a sender that pads its media for rate
/// control.
///
/// Note the padding *length* cannot be used: the RFC 3550 pad count is a single byte, so a payload
/// over 255 bytes is never "all padding" by that measure even when it carries no media, which is
/// exactly the shape these packets have (measured 97..1169 bytes).
fn rtp_carries_media(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && (bytes[0] & 0x20 == 0 || bytes[1] & 0x80 != 0)
}

/// Build an RTCP REMB packet (draft-alvestrand-rmcat-remb) advertising `bitrate_bps` as the
/// receiver's available bandwidth for `media_ssrcs`.
///
/// Why this is needed at all: an SFU decides how much to send an endpoint from the REMB that
/// endpoint reports. Symphony Media Bridge assigns `remb.getBitrate()` straight to its outbound
/// estimate, with no validation or ramp. Browsers (libwebrtc) send REMB and get full rate; GStreamer
/// has no REMB support, so this bridge reports nothing and the SFU leaves it pinned at its
/// configured initial estimate.
///
/// Measured on a 720p25 source: the SFU logged `remb 0kbps` for this endpoint and forwarded roughly
/// half the stream — ~13 fps with slices missing, decoding as smearing on movement, on an idle
/// network. It is worse across a real network path, where the SFU's rate control ratchets *down* on
/// loss and, with no REMB, has no mechanism to climb back — it settles on its configured floor and
/// stays there for the life of the session.
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
    let latency = args.latency;
    let bridge_video = args.bridge_video;
    let video_bitrate = args.video_bitrate;
    let video_preset = args.video_preset;
    let video_key_int = args.video_key_int;
    let (video_width, video_height) = parse_video_size(&args.video_size);
    let video_fps = args.video_fps.max(1);
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
        info!(
            "video bridging enabled: {video_width}x{video_height}@{video_fps}, x264 \
             preset={video_preset}, bitrate={video_bitrate} kbps, \
             key-int-max={video_key_int}, tune=zerolatency"
        );
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

    // Real video is mixed onto a second compositor pad when it arrives (see the video branch of
    // the pad-added handler below).  An earlier attempt used input-selector to switch between
    // black and real video, which stalled mpegtsmux's aggregator on the running-time
    // discontinuity at the pad switch.  compositor keeps a continuous running time across a pad
    // added while PLAYING, so it does not.
    //
    // The cost is that x264 encodes continuously — roughly a core per bridge even with no
    // publisher — and that the output size must be fixed before the source's caps are known.
    let video_branch = if bridge_video {
        build_video_branch(
            video_width,
            video_height,
            video_fps,
            &video_preset,
            video_bitrate,
            video_key_int,
        )
    } else {
        String::new()
    };

    let pipeline_str = format!(
        "{input} audiotestsrc wave=silence is-live=true ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! {mixer} ! avenc_aac ! aacparse ! mux. \
        {video_branch}\
        mpegtsmux name=mux alignment=7 ! queue ! srtsink uri=\"{output_url}\" sync=false wait-for-connection=false latency=100"
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

    // Guard: only the first arriving video pad is bridged; additional video tracks go to fakesink.
    let video_bridged = Arc::new(AtomicBool::new(false));

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

    // SSRCs we are actually receiving, for REMB to report on. Populated by the video selection
    // probe, which already inspects the RTP header.
    let observed_ssrcs: Arc<Mutex<BTreeSet<u32>>> = Arc::new(Mutex::new(BTreeSet::new()));
    let observed_ssrcs_for_remb = Arc::clone(&observed_ssrcs);
    let observed_ssrcs_for_probe = Arc::clone(&observed_ssrcs);

    pipeline.connect_deep_element_added(move |pipe, bin, elem| {
        let elem_type = elem.type_().to_string();
        let _ = pipe;
        let _ = bin;

        if elem_type == "GstRtpBin" {
            info!("setting rtpbin latency to {latency} ms, drop-on-latency=true");
            elem.set_property_from_str("latency", &latency.to_string());
            elem.set_property_from_str("drop-on-latency", "true"); //workaround for large packet_sizing bug in rtpjitterbuffer after long mute + packet loss within first second => stalled audio

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

        let probe_logged = AtomicBool::new(false);
        let observed_ssrcs = Arc::clone(&observed_ssrcs_for_probe);
        pad.add_probe(PadProbeType::BUFFER, move |pad, probe_info| {
            // Does this packet prove the stream carries media? See rtp_carries_media.
            let media_evidence = match probe_info.data {
                Some(gst::PadProbeData::Buffer(ref buffer)) => buffer
                    .map_readable()
                    .ok()
                    .map(|map| {
                        let bytes = map.as_slice();
                        // Recorded here because this probe already has the header mapped. Only used
                        // to populate the REMB report; SMB reads the bitrate regardless of the list.
                        if bytes.len() >= 12 {
                            observed_ssrcs.lock().unwrap().insert(u32::from_be_bytes([
                                bytes[8], bytes[9], bytes[10], bytes[11],
                            ]));
                        }
                        rtp_carries_media(bytes)
                    })
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

            // Logged once per pad: the probe below re-runs for every buffer while a video track is
            // still unproven, and the pad name matters because with more than one video pad it is
            // otherwise impossible to tell from the logs which track was bridged.
            if !probe_logged.swap(true, Ordering::AcqRel) {
                info!("getting {media_type} track on pad '{}'", pad.name());
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
                    // Until a track proves it carries media, drop its buffers and decide nothing.
                    // Dropping is what makes waiting safe: the buffer never reaches the still
                    // unlinked pad, so there is no GST_FLOW_NOT_LINKED and no bus error, and the
                    // single bridge slot stays free for whichever track proves itself first.
                    //
                    // Without this, selection is first-buffer-wins between the real video and any
                    // padding-only stream the SFU also sends (see rtp_carries_media). Bridging the
                    // padding stream produces an SRT output with no video track at all — decodebin
                    // never negotiates caps for it — and no error anywhere to explain why.
                    if bridge_video && !media_evidence {
                        return gstreamer::PadProbeReturn::Drop;
                    }

                    // First video track that proved itself wins; later ones go to fakesink.
                    // video_bridged.swap returns the *previous* value, so the first caller gets false
                    // and proceeds to bridge; all subsequent callers get true and use fakesink.
                    // AcqRel is sufficient: we only need to synchronise this flag, not other memory.
                    if !bridge_video || video_bridged.swap(true, Ordering::AcqRel) {
                        info!(
                            "discarding video pad '{}' to fakesink ({})",
                            pad.name(),
                            if bridge_video {
                                "another video track was already bridged"
                            } else {
                                "video bridging disabled"
                            }
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
                        info!("bridging {encoding_name} video track to SRT output (transcode to H.264)");

                        let pipe_bin = pipeline_clone
                            .dynamic_cast_ref::<gst::Bin>()
                            .expect("could not cast pipeline to bin");

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
                        let pipeline_for_comp = pipeline_clone.clone();

                        decodebin.connect_pad_added(move |_elem, src_pad| {
                            info!("video decodebin src pad added: '{}'", src_pad.name());

                            // Decode → convert → scale into the compositor's fixed output size.
                            // The encoder, h264parse and the mpegtsmux video pad already exist in
                            // the static video branch — that is what keeps video in the PAT/PMT
                            // from the first packet — so all this leg does is mix a source in.
                            //
                            // add-borders keeps the source's aspect ratio, letterboxing into the
                            // configured size instead of stretching to fill it.
                            let videoconvert = ElementFactory::make("videoconvert")
                                .build()
                                .expect("could not create videoconvert");
                            let videoscale = ElementFactory::make("videoscale")
                                .build()
                                .expect("could not create videoscale");
                            videoscale.set_property("add-borders", true);
                            let scale_caps = ElementFactory::make("capsfilter")
                                .build()
                                .expect("could not create video scale capsfilter");
                            scale_caps.set_property_from_str(
                                "caps",
                                &format!("video/x-raw,width={video_width},height={video_height}"),
                            );
                            let queue = ElementFactory::make("queue")
                                .build()
                                .expect("could not create video queue");

                            let elements = [&videoconvert, &videoscale, &scale_caps, &queue];
                            pipe_bin_clone
                                .add_many(elements)
                                .expect("could not add video scale elements to pipeline");
                            for elem in elements {
                                elem.sync_state_with_parent()
                                    .expect("could not sync video scale element state");
                            }
                            gst::Element::link_many(elements)
                                .expect("could not link video scale chain");

                            let comp = pipeline_for_comp
                                .by_name("comp")
                                .expect("could not find compositor");
                            let comp_pad = comp
                                .request_pad_simple("sink_%u")
                                .expect("could not request a compositor pad for the real video");
                            // Above the black fill, which holds sink_0 at the default zorder 0.
                            comp_pad.set_property("zorder", 1u32);
                            info!(
                                "mixing real video onto compositor pad '{}' ({video_width}x{video_height})",
                                comp_pad.name()
                            );
                            queue
                                .static_pad("src")
                                .unwrap()
                                .link(&comp_pad)
                                .expect("could not link video queue src to compositor");

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

    #[test]
    fn parses_a_valid_size() {
        assert_eq!(parse_video_size("1920x1080"), (1920, 1080));
        assert_eq!(parse_video_size("640X480"), (640, 480));
        assert_eq!(parse_video_size(" 1280 x 720 "), (1280, 720));
    }

    #[test]
    fn falls_back_rather_than_failing_on_a_bad_size() {
        // Unattended bridges: a bad setting must not take audio down with it.
        for bad in [
            "", "720p", "1280", "1280x", "x720", "0x720", "-2x720", "abcxdef",
        ] {
            assert_eq!(parse_video_size(bad), (1280, 720), "input {bad:?}");
        }
    }

    #[test]
    fn rejects_odd_dimensions() {
        // Odd dimensions break I420 chroma subsampling.
        assert_eq!(parse_video_size("1281x720"), (1280, 720));
        assert_eq!(parse_video_size("1280x721"), (1280, 720));
    }

    #[test]
    fn video_branch_declares_video_before_the_real_source_arrives() {
        let branch = build_video_branch(1280, 720, 25, "fast", 8000, 60);
        // A black fill feeding the compositor is what holds the video PID in the first PMT.
        assert!(branch.contains("videotestsrc name=videofill pattern=black is-live=true"));
        assert!(branch.contains("compositor name=comp"));
        // The encoder and the mux pad live here, not in the pad-added handler, so they exist
        // before WHEP delivers video.
        assert!(branch.contains("x264enc"));
        assert!(branch.trim_end().ends_with("mux."));
        assert!(branch.contains("width=1280,height=720,framerate=25/1"));
        assert!(branch.contains("speed-preset=fast"));
        assert!(branch.contains("bitrate=8000"));
        assert!(branch.contains("key-int-max=60"));
    }

    /// Build a minimal RTP packet: 12-byte header plus payload, with optional padding and marker
    /// bits, so the tests exercise the same bytes an SFU would put on the wire.
    fn rtp(padding: bool, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 12];
        p[0] = 0x80 | if padding { 0x20 } else { 0 };
        p[1] = 97 | if marker { 0x80 } else { 0 };
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn unpadded_packets_are_media() {
        assert!(rtp_carries_media(&rtp(
            false,
            false,
            &[0x78, 0x00, 0x12, 0x67]
        )));
    }

    #[test]
    fn padded_packets_with_a_marker_are_still_media() {
        // A sender may pad its media for rate control; the marker ends a frame, so this is video.
        assert!(rtp_carries_media(&rtp(true, true, &[0x7c, 0x81, 0xe0])));
    }

    #[test]
    fn padded_packets_without_a_marker_are_not_media() {
        // The shape of a bandwidth-probing packet: padding bit set, no marker, payload of zeros.
        assert!(!rtp_carries_media(&rtp(
            true,
            false,
            &[0x59, 0xf9, 0, 0, 0, 0]
        )));
    }

    #[test]
    fn a_large_padded_payload_is_still_not_media() {
        // Guards against judging by RFC 3550 pad length: over 255 bytes the single-byte pad count
        // cannot describe the whole payload, so a length-based test would wrongly call this media.
        let payload = {
            let mut v = vec![0x59, 0xf9];
            v.extend(std::iter::repeat_n(0u8, 1100));
            v
        };
        assert!(!rtp_carries_media(&rtp(true, false, &payload)));
    }

    #[test]
    fn truncated_buffers_are_not_media() {
        assert!(!rtp_carries_media(&[0x80, 0x61, 0x00]));
        assert!(!rtp_carries_media(&[]));
    }

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
    fn remb_is_valid_with_no_ssrcs_observed_yet() {
        // The bootstrap case: nothing has arrived, but the report must still be well formed —
        // waiting for an SSRC would deadlock, since the SFU sends nothing until it hears an estimate.
        let packet = build_remb(42, &[], 6_000_000);
        let (sender, _, ssrcs) = parse_remb(&packet);
        assert_eq!(sender, 42);
        assert!(ssrcs.is_empty());
        assert_eq!(packet.len(), 20);
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
