use clap::Parser;
use env_logger::Env;
use log::{self, error, info};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
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

    pipeline.connect_deep_element_added(move |pipe, bin, elem| {
        let elem_type = elem.type_().to_string();
        let _ = pipe;
        let _ = bin;

        if elem_type == "GstRtpBin" {
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
                 do-retransmission=true"
            );
            elem.set_property_from_str("latency", &latency.to_string());
            elem.set_property_from_str("drop-on-latency", "false");
            // Enable RTP retransmission: when the jitterbuffer detects a missing sequence
            // number, send NACK upstream and accept the retransmitted packet (RTX, RFC 4588).
            // Browsers do this by default; without it, any lost RTP packet stays lost and
            // produces the classic H.264 decoder corruption we've been chasing (smeared
            // macroblocks, ghost trails) — the source-side SDP advertises rtcp-fb-nack=true
            // so it's prepared to honor the requests.
            elem.set_property_from_str("do-retransmission", "true");
            // Note: do-lost=true was tried but mpegtsmux floods the log with "GAP event
            // outside segment, dropping" warnings — it doesn't know how to handle the
            // GstRTPPacketLost → GstEventGap propagation. The events are useful for raw
            // decoders that can error-conceal, but mpegtsmux is our downstream and it
            // just discards them. Leaving do-lost at its default (false).
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

    let video_preset_for_probe = video_preset.clone();
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

        pad.add_probe(PadProbeType::BUFFER, move |pad, _probe_info| {
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

            info!("getting {media_type} track");
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
                    // Bridge the first video track to mpegtsmux; send additional tracks to fakesink.
                    // video_bridged.swap returns the *previous* value, so the first caller gets false
                    // and proceeds to bridge; all subsequent callers get true and use fakesink.
                    // AcqRel is sufficient: we only need to synchronise this flag, not other memory.
                    if !bridge_video || video_bridged.swap(true, Ordering::AcqRel) {
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
                            info!("bridging H.264 video track to SRT output (passthrough — no transcode)");

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
                            info!("bridging {encoding_name} video track to SRT output (transcode to H.264)");

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

                            decodebin.connect_pad_added(move |_elem, src_pad| {
                                info!("video decodebin src pad added: '{}'", src_pad.name());

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
