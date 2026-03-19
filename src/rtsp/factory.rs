use gstreamer::ClockTime;
use std::{collections::HashMap, time::Duration};

use anyhow::{anyhow, Context, Result};
use gstreamer::{prelude::*, Bin, Caps, Element, ElementFactory, GhostPad};
use gstreamer_app::{AppSrc, AppSrcCallbacks, AppStreamType};
use neolink_core::{
    bc_protocol::StreamKind,
    bcmedia::model::{
        BcMedia, BcMediaIframe, BcMediaInfoV1, BcMediaInfoV2, BcMediaPframe, VideoType,
    },
};
use tokio::{sync::mpsc::channel as mpsc, task::JoinHandle};

use crate::{common::NeoInstance, rtsp::gst::NeoMediaFactory, AnyResult};

#[derive(Clone, Debug)]
pub enum AudioType {
    Aac,
    Adpcm(u32),
}

#[derive(Clone, Debug)]
struct StreamConfig {
    #[allow(dead_code)]
    resolution: [u32; 2],
    bitrate: u32,
    fps: u32,
    bitrate_table: Vec<u32>,
    fps_table: Vec<u32>,
    vid_type: Option<VideoType>,
    aud_type: Option<AudioType>,
    enable_low_latency: bool,
}
impl StreamConfig {
    async fn new(instance: &NeoInstance, name: StreamKind) -> AnyResult<Self> {
        let enable_low_latency = instance.config().await?.borrow().enable_low_latency;
        let (resolution, bitrate, fps, fps_table, bitrate_table) = instance
            .run_passive_task(|cam| {
                Box::pin(async move {
                    let infos = cam
                        .get_stream_info()
                        .await?
                        .stream_infos
                        .iter()
                        .flat_map(|info| info.encode_tables.clone())
                        .collect::<Vec<_>>();
                    if let Some(encode) =
                        infos.iter().find(|encode| encode.name == name.to_string())
                    {
                        let bitrate_table = encode
                            .bitrate_table
                            .split(',')
                            .filter_map(|c| {
                                let i: Result<u32, _> = c.parse();
                                i.ok()
                            })
                            .collect::<Vec<u32>>();
                        let framerate_table = encode
                            .framerate_table
                            .split(',')
                            .filter_map(|c| {
                                let i: Result<u32, _> = c.parse();
                                i.ok()
                            })
                            .collect::<Vec<u32>>();

                        Ok((
                            [encode.resolution.width, encode.resolution.height],
                            bitrate_table
                                .get(encode.default_bitrate as usize)
                                .copied()
                                .unwrap_or(encode.default_bitrate)
                                * 1024,
                            framerate_table
                                .get(encode.default_framerate as usize)
                                .copied()
                                .unwrap_or(encode.default_framerate),
                            framerate_table.clone(),
                            bitrate_table.clone(),
                        ))
                    } else {
                        Ok(([0, 0], 0, 0, vec![], vec![]))
                    }
                })
            })
            .await?;

        Ok(StreamConfig {
            resolution,
            bitrate,
            fps,
            fps_table,
            bitrate_table,
            vid_type: None,
            aud_type: None,
            enable_low_latency,
        })
    }

    fn update_fps(&mut self, fps: u32) {
        let new_fps = self.fps_table.get(fps as usize).copied().unwrap_or(fps);
        self.fps = new_fps;
    }
    #[allow(dead_code)]
    fn update_bitrate(&mut self, bitrate: u32) {
        let new_bitrate = self
            .bitrate_table
            .get(bitrate as usize)
            .copied()
            .unwrap_or(bitrate);
        self.bitrate = new_bitrate;
    }

    fn update_from_media(&mut self, media: &BcMedia) {
        match media {
            BcMedia::InfoV1(BcMediaInfoV1 { fps, .. })
            | BcMedia::InfoV2(BcMediaInfoV2 { fps, .. }) => self.update_fps(*fps as u32),
            BcMedia::Aac(_) => {
                self.aud_type = Some(AudioType::Aac);
            }
            BcMedia::Adpcm(adpcm) => {
                self.aud_type = Some(AudioType::Adpcm(adpcm.block_size()));
            }
            BcMedia::Iframe(BcMediaIframe { video_type, .. })
            | BcMedia::Pframe(BcMediaPframe { video_type, .. }) => {
                self.vid_type = Some(*video_type);
            }
            BcMedia::RawReplayChunk(_) => {}
            BcMedia::ReplayStarted(_) => {} // replay-only; RTSP ignores
            BcMedia::StreamEnd => {}
        }
    }
}

pub(super) async fn make_dummy_factory(
    use_splash: bool,
    pattern: String,
) -> AnyResult<NeoMediaFactory> {
    NeoMediaFactory::new_with_callback(move |element| {
        clear_bin(&element)?;
        if !use_splash {
            Ok(None)
        } else {
            build_unknown(&element, &pattern)?;
            Ok(Some(element))
        }
    })
    .await
}

enum ClientMsg {
    NewClient {
        element: Element,
        reply: tokio::sync::oneshot::Sender<Element>,
    },
}

pub(super) async fn make_factory(
    camera: NeoInstance,
    stream: StreamKind,
) -> AnyResult<(NeoMediaFactory, JoinHandle<AnyResult<()>>)> {
    let (client_tx, mut client_rx) = mpsc(100);
    // Create the task that creates the pipelines
    let thread = tokio::task::spawn(async move {
        let name = camera.config().await?.borrow().name.clone();

        while let Some(msg) = client_rx.recv().await {
            match msg {
                ClientMsg::NewClient { element, reply } => {
                    log::debug!("New client for {name}::{stream}");
                    let camera = camera.clone();
                    let name = name.clone();
                    tokio::task::spawn(async move {
                        clear_bin(&element)?;
                        log::trace!("{name}::{stream}: Starting camera");

                        // Start the camera
                        let config = camera.config().await?.borrow().clone();
                        let mut media_rx = camera.stream_while_live(stream).await?;

                        log::trace!("{name}::{stream}: Learning camera stream type");
                        // Learn the camera data type
                        let mut buffer = vec![];
                        let mut frame_count = 0usize;

                        let mut stream_config = StreamConfig::new(&camera, stream).await?;
                        while let Some(media) = media_rx.recv().await {
                            stream_config.update_from_media(&media);
                            buffer.push(media);
                            if stream_config.enable_low_latency {
                                if frame_count > 5
                                    || (stream_config.vid_type.is_some()
                                        && stream_config.aud_type.is_some())
                                {
                                    break;
                                }
                            } else if frame_count > 10
                                || (stream_config.vid_type.is_some()
                                    && stream_config.aud_type.is_some())
                            {
                                break;
                            }
                            frame_count += 1;
                        }

                        log::trace!("{name}::{stream}: Building the pipeline");
                        // Build the right video pipeline
                        let vid_src = match stream_config.vid_type.as_ref() {
                            Some(VideoType::H264) => {
                                let src = build_h264(&element, &stream_config)?;
                                AnyResult::Ok(Some(src))
                            }
                            Some(VideoType::H265) => {
                                let src = build_h265(&element, &stream_config)?;
                                AnyResult::Ok(Some(src))
                            }
                            None => {
                                build_unknown(&element, &config.splash_pattern.to_string())?;
                                AnyResult::Ok(None)
                            }
                        }?;

                        // Build the right audio pipeline
                        let aud_src = if config.enable_audio {
                            match stream_config.aud_type.as_ref() {
                                Some(AudioType::Aac) => {
                                    let src = build_aac(&element, &stream_config)?;
                                    AnyResult::Ok(Some(src))
                                }
                                Some(AudioType::Adpcm(block_size)) => {
                                    let src =
                                        build_adpcm(&element, *block_size, &stream_config)?;
                                    AnyResult::Ok(Some(src))
                                }
                                None => AnyResult::Ok(None),
                            }?
                        } else {
                            None
                        };

                        if let Some(app) = vid_src.as_ref() {
                            app.set_callbacks(
                                AppSrcCallbacks::builder()
                                    .seek_data(move |_, _seek_pos| true)
                                    .build(),
                            );
                        }
                        if let Some(app) = aud_src.as_ref() {
                            app.set_callbacks(
                                AppSrcCallbacks::builder()
                                    .seek_data(move |_, _seek_pos| true)
                                    .build(),
                            );
                        }

                        log::trace!("{name}::{stream}: Sending pipeline to gstreamer");
                        // Send the pipeline back to the factory so it can start
                        let _ = reply.send(element);

                        // Run blocking code on a seperate thread
                        // This is not an async thread
                        std::thread::spawn(move || {
                            let mut aud_ts: u64 = 0;
                            let mut vid_ts: u64 = 0;
                            let mut pools = Default::default();

                            log::trace!("{name}::{stream}: Sending buffered frames");
                            for buffered in buffer.drain(..) {
                                send_to_sources(
                                    buffered,
                                    &mut pools,
                                    &vid_src,
                                    &aud_src,
                                    &mut vid_ts,
                                    &mut aud_ts,
                                    &stream_config,
                                )?;
                            }

                            log::trace!("{name}::{stream}: Sending new frames");
                            while let Some(data) = media_rx.blocking_recv() {
                                let frames = if stream_config.enable_low_latency {
                                    drain_to_latest(data, &mut media_rx)
                                } else {
                                    vec![data]
                                };

                                for frame in frames {
                                    let r = send_to_sources(
                                        frame,
                                        &mut pools,
                                        &vid_src,
                                        &aud_src,
                                        &mut vid_ts,
                                        &mut aud_ts,
                                        &stream_config,
                                    );
                                    if let Err(r) = &r {
                                        log::info!("Failed to send to source: {r:?}");
                                    }
                                    r?;
                                }
                            }
                            log::trace!("All media recieved");
                            AnyResult::Ok(())
                        });
                        AnyResult::Ok(())
                    });
                }
            }
        }
        AnyResult::Ok(())
    });

    // Now setup the factory
    let factory = NeoMediaFactory::new_with_callback(move |element| {
        let (reply, new_element) = tokio::sync::oneshot::channel();
        client_tx.blocking_send(ClientMsg::NewClient { element, reply })?;

        let element = new_element.blocking_recv()?;
        Ok(Some(element))
    })
    .await?;
    Ok((factory, thread))
}

/// In low-latency mode, drain all immediately-available frames from the channel
/// and return only the latest decodable set (from the last I-frame onward).
/// If nothing was queued, returns the original frame as-is (zero overhead).
fn drain_to_latest(
    first: BcMedia,
    rx: &mut tokio::sync::mpsc::Receiver<BcMedia>,
) -> Vec<BcMedia> {
    let mut frames = vec![first];
    while let Ok(frame) = rx.try_recv() {
        frames.push(frame);
    }

    if frames.len() <= 1 {
        return frames;
    }

    let last_iframe_idx = frames.iter().rposition(|f| matches!(f, BcMedia::Iframe(_)));

    match last_iframe_idx {
        Some(idx) => {
            if idx > 0 {
                log::info!(
                    "Low-latency: dropping {} stale frames, skipping to latest I-frame",
                    idx
                );
            }
            frames.split_off(idx)
        }
        None => {
            // No I-frame to resync from — keep all frames to maintain
            // the P-frame decode chain (each depends on the previous)
            frames
        }
    }
}

fn send_to_sources(
    data: BcMedia,
    pools: &mut HashMap<usize, gstreamer::BufferPool>,
    vid_src: &Option<AppSrc>,
    aud_src: &Option<AppSrc>,
    vid_ts: &mut u64,
    aud_ts: &mut u64,
    stream_config: &StreamConfig,
) -> AnyResult<()> {
    // Update TS
    match data {
        BcMedia::Aac(aac) => {
            let duration = aac.duration().expect("Could not calculate AAC duration");
            if let Some(aud_src) = aud_src.as_ref() {
                let max = aud_src.max_bytes();
                if max > 0 && aud_src.current_level_bytes() >= max * 9 / 10 {
                    log::debug!("Audio buffer near capacity, dropping AAC frame");
                } else {
                    log::debug!("Sending AAC: {:?}", Duration::from_micros(*aud_ts));
                    send_to_appsrc(
                        aud_src,
                        aac.data,
                        Duration::from_micros(*aud_ts),
                        pools,
                        stream_config.enable_low_latency,
                    )?;
                }
            }
            *aud_ts += duration as u64;
        }
        BcMedia::Adpcm(adpcm) => {
            let duration = adpcm
                .duration()
                .expect("Could not calculate ADPCM duration");
            if let Some(aud_src) = aud_src.as_ref() {
                let max = aud_src.max_bytes();
                if max > 0 && aud_src.current_level_bytes() >= max * 9 / 10 {
                    log::debug!("Audio buffer near capacity, dropping ADPCM frame");
                } else {
                    log::trace!("Sending ADPCM: {:?}", Duration::from_micros(*aud_ts));
                    send_to_appsrc(
                        aud_src,
                        adpcm.data,
                        Duration::from_micros(*aud_ts),
                        pools,
                        stream_config.enable_low_latency,
                    )?;
                }
            }
            *aud_ts += duration as u64;
        }
        BcMedia::Iframe(BcMediaIframe { data, .. })
        | BcMedia::Pframe(BcMediaPframe { data, .. }) => {
            if let Some(vid_src) = vid_src.as_ref() {
                log::trace!("Sending VID: {:?}", Duration::from_micros(*vid_ts));
                send_to_appsrc(
                    vid_src,
                    data,
                    Duration::from_micros(*vid_ts),
                    pools,
                    stream_config.enable_low_latency,
                )?;
            }
            const MICROSECONDS: u64 = 1000000;
            *vid_ts += MICROSECONDS / stream_config.fps as u64;
        }
        _ => {}
    }
    Ok(())
}

fn bucket_size_for(n: usize) -> Option<usize> {
    const MIN_BUCKET: usize = 256;
    const MAX_BUCKET: usize = 1024 * 1024;
    if n == 0 {
        return Some(MIN_BUCKET);
    }
    if n > MAX_BUCKET {
        return None;
    }
    let mut b = n.next_power_of_two();
    if b < MIN_BUCKET {
        b = MIN_BUCKET;
    }
    Some(b)
}

fn acquire_pooled_buffer(
    pools: &mut std::collections::HashMap<usize, gstreamer::BufferPool>,
    data: &[u8],
    timestamp: gstreamer::ClockTime,
) -> AnyResult<gstreamer::Buffer> {
    let needed = data.len();
    if let Some(bucket) = bucket_size_for(needed) {
        let pool = pools.entry(bucket).or_insert_with(|| {
            let pool = gstreamer::BufferPool::new();
            let mut cfg = pool.config();
            cfg.set_params(None, bucket as u32, 8, 64);
            pool.set_config(cfg).expect("pool config failed");
            pool.set_active(true).expect("activate pool");
            log::info!("New BufferPool (Bucket) allocated: size={bucket}");
            pool
        });

        let mut buf = pool.acquire_buffer(None)?;
        {
            let buf_ref = buf.get_mut().unwrap();
            buf_ref.set_dts(timestamp);
            buf_ref.set_pts(timestamp);
            {
                let mut map = buf_ref.map_writable().unwrap();
                map[..needed].copy_from_slice(data);
            }
            if bucket > needed {
                let _ = buf_ref.set_size(needed);
            }
        }
        Ok(buf)
    } else {
        let mut buf = gstreamer::Buffer::with_size(needed)
            .context("allocate large non-pooled buffer")?;
        {
            let buf_ref = buf.get_mut().unwrap();
            buf_ref.set_dts(timestamp);
            buf_ref.set_pts(timestamp);
            let mut map = buf_ref.map_writable().unwrap();
            map.copy_from_slice(data);
        }
        Ok(buf)
    }
}

fn send_to_appsrc(
    appsrc: &AppSrc,
    data: Vec<u8>,
    mut ts: Duration,
    pools: &mut HashMap<usize, gstreamer::BufferPool>,
    low_latency: bool,
) -> AnyResult<()> {
    check_live(appsrc)?;

    if appsrc.is_live() {
        if let Some(time) = appsrc
            .current_clock_time()
            .and_then(|t| appsrc.base_time().map(|bt| t - bt))
        {
            if matches!(appsrc.current_state(), gstreamer::State::Playing) {
                ts = Duration::from_micros(time.useconds());
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        }
    }

    let timestamp = ClockTime::from_useconds(ts.as_micros() as u64);
    let buf = acquire_pooled_buffer(pools, &data, timestamp)?;

    match appsrc.push_buffer(buf) {
        Ok(_) => {}
        Err(gstreamer::FlowError::Flushing) => {
            log::info!(
                "Buffer full on {} pausing stream until client consumes frames",
                appsrc.name()
            );
            return Ok(());
        }
        Err(e) => return Err(anyhow!("Error in streaming: {e:?}")),
    }

    if !low_latency {
        let level = appsrc.current_level_bytes();
        let max = appsrc.max_bytes();
        if level >= max * 2 / 3 && matches!(appsrc.current_state(), gstreamer::State::Paused) {
            let _ = appsrc.set_state(gstreamer::State::Playing);
        } else if level <= max / 3 && matches!(appsrc.current_state(), gstreamer::State::Playing)
        {
            let _ = appsrc.set_state(gstreamer::State::Paused);
        }
    }

    Ok(())
}
fn check_live(app: &AppSrc) -> Result<()> {
    app.bus().ok_or(anyhow!("App source is closed"))?;
    app.pads()
        .iter()
        .all(|pad| pad.is_linked())
        .then_some(())
        .ok_or(anyhow!("App source is not linked"))
}

fn clear_bin(bin: &Element) -> Result<()> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    // Clear the autogenerated ones
    for element in bin.iterate_elements().into_iter().flatten() {
        bin.remove(&element)?;
    }

    Ok(())
}

fn build_unknown(bin: &Element, pattern: &str) -> Result<()> {
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Unknown Pipeline");
    let source = make_element("videotestsrc", "testvidsrc")?;
    source.set_property_from_str("pattern", pattern);
    source.set_property("num-buffers", 500i32); // Send buffers then EOS
    let queue = make_queue("queue0", 1024 * 1024 * 4, false)?;

    let overlay = make_element("textoverlay", "overlay")?;
    overlay.set_property("text", "Stream not Ready");
    overlay.set_property_from_str("valignment", "top");
    overlay.set_property_from_str("halignment", "left");
    overlay.set_property("font-desc", "Sans, 16");
    let encoder = make_element("jpegenc", "encoder")?;
    let payload = make_element("rtpjpegpay", "pay0")?;

    bin.add_many([&source, &queue, &overlay, &encoder, &payload])?;
    source.link_filtered(
        &queue,
        &Caps::builder("video/x-raw")
            .field("format", "YUY2")
            .field("width", 896i32)
            .field("height", 512i32)
            .field("framerate", gstreamer::Fraction::new(25, 1))
            .build(),
    )?;
    Element::link_many([&queue, &overlay, &encoder, &payload])?;

    Ok(())
}

struct Linked {
    appsrc: AppSrc,
    output: Element,
}

fn pipe_h264(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = buffer_size(stream_config.bitrate, stream_config.enable_low_latency);
    log::debug!(
        "buffer_size: {buffer_size}, bitrate: {}",
        stream_config.bitrate
    );
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building H264 Pipeline");
    let source = make_element("appsrc", "vidsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;

    source.set_is_live(stream_config.enable_low_latency);
    source.set_block(false);
    if stream_config.enable_low_latency {
        source.set_min_latency(1000 / (stream_config.fps as i64));
    }
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(stream_config.enable_low_latency);
    source.set_stream_type(AppStreamType::Stream);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;
    let queue = make_queue("source_queue", buffer_size, stream_config.enable_low_latency)?;
    let parser = make_element("h264parse", "parser")?;
    if stream_config.enable_low_latency {
        parser.set_property("config-interval", -1i32);
    }
    // let stamper = make_element("h264timestamper", "stamper")?;

    bin.add_many([&source, &queue, &parser])?;
    Element::link_many([&source, &queue, &parser])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: parser,
    })
}

fn build_h264(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_h264(bin, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtph264pay", "pay0")?;
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

fn pipe_h265(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = buffer_size(stream_config.bitrate, stream_config.enable_low_latency);
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building H265 Pipeline");
    let source = make_element("appsrc", "vidsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;
    source.set_is_live(stream_config.enable_low_latency);
    source.set_block(false);
    if stream_config.enable_low_latency {
        source.set_min_latency(1000 / (stream_config.fps as i64));
    }
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(stream_config.enable_low_latency);
    source.set_stream_type(AppStreamType::Stream);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;
    let queue = make_queue("source_queue", buffer_size, stream_config.enable_low_latency)?;
    let parser = make_element("h265parse", "parser")?;
    if stream_config.enable_low_latency {
        parser.set_property("config-interval", -1i32);
    }
    // let stamper = make_element("h265timestamper", "stamper")?;

    bin.add_many([&source, &queue, &parser])?;
    Element::link_many([&source, &queue, &parser])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: parser,
    })
}

fn build_h265(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_h265(bin, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtph265pay", "pay0")?;
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

fn pipe_aac(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = if stream_config.enable_low_latency {
        8 * 1024 // ~500ms of 8kHz mono audio
    } else {
        512 * 1416
    };
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Aac pipeline");
    let source = make_element("appsrc", "audsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;

    source.set_is_live(stream_config.enable_low_latency);
    source.set_block(false);
    if stream_config.enable_low_latency {
        source.set_min_latency(1000 / (stream_config.fps as i64));
    }
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(stream_config.enable_low_latency);
    source.set_stream_type(AppStreamType::Stream);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let queue = make_queue("audqueue", buffer_size, stream_config.enable_low_latency)?;
    let parser = make_element("aacparse", "audparser")?;
    let decoder = match make_element("faad", "auddecoder_faad") {
        Ok(ele) => Ok(ele),
        Err(_) => make_element("avdec_aac", "auddecoder_avdec_aac"),
    }?;

    // The fallback
    let silence = make_element("audiotestsrc", "audsilence")?;
    silence.set_property_from_str("wave", "silence");
    let fallback_switch = make_element("fallbackswitch", "audfallbackswitch");
    if let Ok(fallback_switch) = fallback_switch.as_ref() {
        let fb_timeout = if stream_config.enable_low_latency {
            500_000_000u64 // 500ms
        } else {
            3_000_000_000u64 // 3s
        };
        fallback_switch.set_property("timeout", fb_timeout);
        fallback_switch.set_property("immediate-fallback", true);
    }

    let encoder = make_element("audioconvert", "audencoder")?;

    bin.add_many([&source, &queue, &parser, &decoder, &encoder])?;
    if let Ok(fallback_switch) = fallback_switch.as_ref() {
        bin.add_many([&silence, fallback_switch])?;
        Element::link_many([
            &source,
            &queue,
            &parser,
            &decoder,
            fallback_switch,
            &encoder,
        ])?;
        Element::link_many([&silence, fallback_switch])?;
    } else {
        Element::link_many([&source, &queue, &parser, &decoder, &encoder])?;
    }

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: encoder,
    })
}

fn build_aac(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_aac(bin, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtpL16pay", "pay1")?;
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

fn pipe_adpcm(bin: &Element, block_size: u32, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = if stream_config.enable_low_latency {
        8 * 1024 // ~500ms of 8kHz mono audio
    } else {
        512 * 1416
    };
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Adpcm pipeline");
    // Original command line
    // caps=audio/x-adpcm,layout=dvi,block_align={},channels=1,rate=8000
    // ! queue silent=true max-size-bytes=10485760 min-threshold-bytes=1024
    // ! adpcmdec
    // ! audioconvert
    // ! rtpL16pay name=pay1

    let source = make_element("appsrc", "audsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;
    source.set_is_live(stream_config.enable_low_latency);
    source.set_block(false);
    if stream_config.enable_low_latency {
        source.set_min_latency(1000 / (stream_config.fps as i64));
    }
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(stream_config.enable_low_latency);
    source.set_stream_type(AppStreamType::Stream);

    source.set_caps(Some(
        &Caps::builder("audio/x-adpcm")
            .field("layout", "div")
            .field("block_align", block_size as i32)
            .field("channels", 1i32)
            .field("rate", 8000i32)
            .build(),
    ));

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let queue = make_queue("audqueue", buffer_size, stream_config.enable_low_latency)?;
    let decoder = make_element("decodebin", "auddecoder")?;
    let encoder = make_element("audioconvert", "audencoder")?;
    let encoder_out = encoder.clone();

    bin.add_many([&source, &queue, &decoder, &encoder])?;
    Element::link_many([&source, &queue, &decoder])?;
    decoder.connect_pad_added(move |_element, pad| {
        let sink_pad = encoder
            .static_pad("sink")
            .expect("Encoder is missing its pad");
        pad.link(&sink_pad)
            .expect("Failed to link ADPCM decoder to encoder");
    });

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: encoder_out,
    })
}

fn build_adpcm(bin: &Element, block_size: u32, stream_config: &StreamConfig) -> Result<AppSrc> {
    let linked = pipe_adpcm(bin, block_size, stream_config)?;

    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;

    let payload = make_element("rtpL16pay", "pay1")?;
    bin.add_many([&payload])?;
    Element::link_many([&linked.output, &payload])?;
    Ok(linked.appsrc)
}

#[allow(dead_code)]
fn pipe_silence(bin: &Element, stream_config: &StreamConfig) -> Result<Linked> {
    let buffer_size = if stream_config.enable_low_latency {
        8 * 1024 // ~500ms of 8kHz mono audio
    } else {
        512 * 1416
    };
    let bin = bin
        .clone()
        .dynamic_cast::<Bin>()
        .map_err(|_| anyhow!("Media source's element should be a bin"))?;
    log::debug!("Building Silence pipeline");
    let source = make_element("appsrc", "audsrc")?
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot cast to appsrc."))?;

    source.set_is_live(stream_config.enable_low_latency);
    source.set_block(false);
    if stream_config.enable_low_latency {
        source.set_min_latency(1000 / (stream_config.fps as i64));
    }
    source.set_property("emit-signals", false);
    source.set_max_bytes(buffer_size as u64);
    source.set_do_timestamp(stream_config.enable_low_latency);
    source.set_stream_type(AppStreamType::Stream);

    let source = source
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot cast back"))?;

    let sink_queue = make_queue("audsinkqueue", buffer_size, stream_config.enable_low_latency)?;
    let sink = make_element("fakesink", "silence_sink")?;

    let silence = make_element("audiotestsrc", "audsilence")?;
    silence.set_property_from_str("wave", "silence");
    let src_queue = make_queue("audsinkqueue", buffer_size, stream_config.enable_low_latency)?;
    let encoder = make_element("audioconvert", "audencoder")?;

    bin.add_many([&source, &sink_queue, &sink, &silence, &src_queue, &encoder])?;

    Element::link_many([&source, &sink_queue, &sink])?;

    Element::link_many([&silence, &src_queue, &encoder])?;

    let source = source
        .dynamic_cast::<AppSrc>()
        .map_err(|_| anyhow!("Cannot convert appsrc"))?;
    Ok(Linked {
        appsrc: source,
        output: encoder,
    })
}

#[allow(dead_code)]
struct AppSrcPair {
    vid: AppSrc,
    aud: Option<AppSrc>,
}

// #[allow(dead_code)]
// /// Experimental build a stream of MPEGTS
// fn build_mpegts(bin: &Element, stream_config: &StreamConfig) -> Result<AppSrcPair> {
//     let buffer_size = buffer_size(stream_config.bitrate);
//     log::debug!(
//         "buffer_size: {buffer_size}, bitrate: {}",
//         stream_config.bitrate
//     );

//     // VID
//     let vid_link = match stream_config.vid_format {
//         VidFormat::H264 => pipe_h264(bin, stream_config)?,
//         VidFormat::H265 => pipe_h265(bin, stream_config)?,
//         VidFormat::None => unreachable!(),
//     };

//     // AUD
//     let aud_link = match stream_config.aud_format {
//         AudFormat::Aac => pipe_aac(bin, stream_config)?,
//         AudFormat::Adpcm(block) => pipe_adpcm(bin, block, stream_config)?,
//         AudFormat::None => pipe_silence(bin, stream_config)?,
//     };

//     let bin = bin
//         .clone()
//         .dynamic_cast::<Bin>()
//         .map_err(|_| anyhow!("Media source's element should be a bin"))?;

//     // MUX
//     let muxer = make_element("mpegtsmux", "mpeg_muxer")?;
//     let rtp = make_element("rtpmp2tpay", "pay0")?;

//     bin.add_many([&muxer, &rtp])?;
//     Element::link_many([&vid_link.output, &muxer, &rtp])?;
//     Element::link_many([&aud_link.output, &muxer])?;

//     Ok(AppSrcPair {
//         vid: vid_link.appsrc,
//         aud: Some(aud_link.appsrc),
//     })
// }

// Convenice funcion to make an element or provide a message
// about what plugin is missing
fn make_element(kind: &str, name: &str) -> AnyResult<Element> {
    ElementFactory::make_with_name(kind, Some(name)).with_context(|| {
        let plugin = match kind {
            "appsrc" => "app (gst-plugins-base)",
            "audioconvert" => "audioconvert (gst-plugins-base)",
            "adpcmdec" => "Required for audio",
            "h264parse" => "videoparsersbad (gst-plugins-bad)",
            "h265parse" => "videoparsersbad (gst-plugins-bad)",
            "rtph264pay" => "rtp (gst-plugins-good)",
            "rtph265pay" => "rtp (gst-plugins-good)",
            "rtpjitterbuffer" => "rtp (gst-plugins-good)",
            "aacparse" => "audioparsers (gst-plugins-good)",
            "rtpL16pay" => "rtp (gst-plugins-good)",
            "x264enc" => "x264 (gst-plugins-ugly)",
            "x265enc" => "x265 (gst-plugins-bad)",
            "avdec_h264" => "libav (gst-libav)",
            "avdec_h265" => "libav (gst-libav)",
            "videotestsrc" => "videotestsrc (gst-plugins-base)",
            "imagefreeze" => "imagefreeze (gst-plugins-good)",
            "audiotestsrc" => "audiotestsrc (gst-plugins-base)",
            "decodebin" => "playback (gst-plugins-good)",
            _ => "Unknown",
        };
        format!(
            "Missing required gstreamer plugin `{}` for `{}` element",
            plugin, kind
        )
    })
}

#[allow(dead_code)]
fn make_dbl_queue(name: &str, buffer_size: u32) -> AnyResult<Element> {
    let queue = make_element("queue", &format!("queue1_{}", name))?;
    queue.set_property("max-size-bytes", buffer_size);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", 0u64);
    // queue.set_property(
    //     "max-size-time",
    //     std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
    //         .unwrap_or(0),
    // );

    let queue2 = make_element("queue2", &format!("queue2_{}", name))?;
    queue2.set_property("max-size-bytes", buffer_size * 2u32 / 3u32);
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue2.set_property(
        "max-size-time",
        std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
            .unwrap_or(0),
    );
    queue2.set_property("use-buffering", false);

    let bin = gstreamer::Bin::builder().name(name).build();
    bin.add_many([&queue, &queue2])?;
    Element::link_many([&queue, &queue2])?;

    let pad = queue
        .static_pad("sink")
        .expect("Failed to get a static pad from queue.");
    let ghost_pad = GhostPad::builder_with_target(&pad).unwrap().build();
    ghost_pad.set_active(true)?;
    bin.add_pad(&ghost_pad)?;

    let pad = queue2
        .static_pad("src")
        .expect("Failed to get a static pad from queue2.");
    let ghost_pad = GhostPad::builder_with_target(&pad).unwrap().build();
    ghost_pad.set_active(true)?;
    bin.add_pad(&ghost_pad)?;

    let bin = bin
        .dynamic_cast::<Element>()
        .map_err(|_| anyhow!("Cannot convert bin"))?;
    Ok(bin)
}

fn make_queue(name: &str, buffer_size: u32, low_latency: bool) -> AnyResult<Element> {
    let queue = make_element("queue", &format!("queue1_{}", name))?;
    queue.set_property("max-size-bytes", buffer_size);
    queue.set_property("max-size-buffers", 0u32);
    if low_latency {
        // 200ms max queue time — ~5 frames at 25fps
        queue.set_property("max-size-time", 200_000_000u64);
    } else {
        queue.set_property(
            "max-size-time",
            std::convert::TryInto::<u64>::try_into(tokio::time::Duration::from_secs(5).as_nanos())
                .unwrap_or(0),
        );
    }
    Ok(queue)
}

fn buffer_size(bitrate: u32, low_latency: bool) -> u32 {
    if low_latency {
        // ~1 frame worth of buffer
        std::cmp::max(bitrate / 8u32, 4u32 * 1024u32)
    } else {
        // ~0.25 seconds of buffer
        std::cmp::max(bitrate * 2 / 8u32, 4u32 * 1024u32)
    }
}
