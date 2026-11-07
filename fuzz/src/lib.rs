//! Shared scaffolding for the structure-aware MP4 / ISO BMFF fuzz
//! targets.
//!
//! Three building blocks:
//!
//! * [`Recipe`] — a bounded byte-reader that turns the fuzzer's input
//!   into structured decisions (counts, sizes, enum picks). Reads past
//!   the end return zeros so every prefix of an input is a valid
//!   recipe.
//! * [`MuxPlan`] — a valid-by-construction muxing recipe: 1–2 tracks
//!   (stereo/mono s16 PCM, arbitrary-payload video), plain /
//!   faststart / fragmented layouts (with optional `styp` + `sidx` /
//!   `mfra` random-access indexes, empty-time inserts, and per-segment
//!   `emsg` events), start-delay edit lists, and recipe-driven packet
//!   sizes / timestamps. `run()` executes the plan through our own
//!   muxer; a muxer error on a plan this module constructed is a
//!   contract violation and panics.
//! * [`exercise_demux`] — the full hostile-input demux battery: the
//!   typed open front, every public accessor (CENC / PIFF / emsg /
//!   HEIF item catalogue / edit lists / fragment records), a bounded
//!   packet drain, and both seek paths. Whether the input opens or
//!   errors is the input's business; panics, aborts, debug-build
//!   overflow, and attacker-proportional allocations are the bugs.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Demuxer as _, Muxer as _, NullCodecResolver, Packet, ReadSeek,
    SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_mp4::options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

// ───────────────────────────── Recipe ─────────────────────────────

/// Bounded reader over the fuzz input. Reads past the end yield zeros,
/// so short inputs still decode to a complete (if boring) plan.
pub struct Recipe<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Recipe<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Recipe { data, pos: 0 }
    }

    pub fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos = self.pos.saturating_add(1);
        b
    }

    pub fn u16(&mut self) -> u16 {
        u16::from_be_bytes([self.u8(), self.u8()])
    }

    pub fn u32(&mut self) -> u32 {
        u32::from_be_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }

    pub fn u64(&mut self) -> u64 {
        ((self.u32() as u64) << 32) | self.u32() as u64
    }

    /// Up to `n` bytes (fewer when the input is exhausted).
    pub fn take(&mut self, n: usize) -> Vec<u8> {
        let end = (self.pos + n).min(self.data.len());
        let out = self.data[self.pos.min(self.data.len())..end].to_vec();
        self.pos = end.max(self.pos);
        out
    }

    /// Exactly `n` bytes, zero-padded past the end of the input.
    pub fn take_exact(&mut self, n: usize) -> Vec<u8> {
        let mut v = self.take(n);
        v.resize(n, 0);
        v
    }

    /// The unread remainder of the input.
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos.min(self.data.len())..]
    }
}

// ─────────────────────── Shared output buffer ───────────────────────

/// `Write + Seek + Send` sink whose bytes stay reachable after the
/// muxer consumes the `Box<dyn WriteSeek>`.
struct SharedBuf {
    buf: Arc<Mutex<Vec<u8>>>,
    pos: u64,
}

impl Write for SharedBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut buf = self.buf.lock().unwrap();
        let pos = self.pos as usize;
        if pos + data.len() > buf.len() {
            buf.resize(pos + data.len(), 0);
        }
        buf[pos..pos + data.len()].copy_from_slice(data);
        self.pos += data.len() as u64;
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for SharedBuf {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let len = self.buf.lock().unwrap().len() as i64;
        let target = match from {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(off) => len + off,
            SeekFrom::Current(off) => self.pos as i64 + off,
        };
        if target < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

// ───────────────────────────── MuxPlan ─────────────────────────────

/// One track's expected round-trip surface.
pub struct TrackExpect {
    /// Payload bytes per packet, in write order.
    pub payloads: Vec<Vec<u8>>,
    /// PTS per packet, in write order (track time base).
    pub pts: Vec<i64>,
    /// Denominator of the track's `1/den` input time base (the scale
    /// `pts` is expressed in).
    pub tb_den: i64,
}

/// A file built by [`MuxPlan::run`] plus the identity contract the
/// demuxer must reproduce.
pub struct Built {
    pub bytes: Vec<u8>,
    pub tracks: Vec<TrackExpect>,
    /// When `false`, only payload identity is asserted (empty-time
    /// inserts legitimately shift downstream timestamps).
    pub check_pts: bool,
}

enum TrackKind {
    Pcm,
    Mjpeg,
}

struct TrackPlan {
    kind: TrackKind,
    stream: StreamInfo,
    packets: Vec<Packet>,
    /// Ticks per second of the track time base (for cross-track
    /// interleave ordering).
    ticks_per_sec: i64,
}

struct FragPlan {
    options: FragmentedOptions,
    /// `(track_index, after_packet_index, gap_duration)` empty-time
    /// inserts, applied in write order.
    empty_time: Vec<(usize, usize, u32)>,
    /// Queue one `emsg` before the first fragment when set.
    emsg: Option<oxideav_mp4::emsg::EmsgBox>,
}

/// Valid-by-construction muxing recipe. See the module docs.
pub struct MuxPlan {
    tracks: Vec<TrackPlan>,
    options: Mp4MuxerOptions,
    frag: Option<FragPlan>,
    check_pts: bool,
}

/// Tracks carry timestamps that rescale *exactly* through the muxer's
/// media timescales (audio: the sample rate; video: 1000) and the
/// movie timescale (1000) — the start delay is expressed in whole
/// multiples of 10 ms so every supported rate (including 44100)
/// converts without truncation, keeping the pts identity assertion
/// exact instead of tolerance-based.
fn pcm_track(r: &mut Recipe, index: usize, start_delay_ms: i64) -> TrackPlan {
    let channels = 1 + (r.u8() % 2) as u16;
    let rate = [8_000u32, 16_000, 44_100, 48_000][(r.u8() % 4) as usize];
    let mut params = CodecParameters::audio(CodecId::new("pcm_s16le"));
    params.channels = Some(channels);
    params.sample_rate = Some(rate);
    params.sample_format = Some(SampleFormat::S16);
    let stream = StreamInfo {
        index: index as u32,
        time_base: TimeBase::new(1, rate as i64),
        duration: None,
        start_time: Some(0),
        params,
    };

    let n_pkts = 1 + (r.u8() % 6) as usize;
    let mut packets = Vec::with_capacity(n_pkts);
    // Exact for every supported rate: rate * 10ms is a whole tick count.
    let mut pts = start_delay_ms * rate as i64 / 1_000;
    for _ in 0..n_pkts {
        let frames = 1 + (r.u8() % 64) as usize;
        let mut payload = r.take(frames * channels as usize * 2);
        payload.resize(frames * channels as usize * 2, 0x5a);
        let mut p = Packet::new(index as u32, stream.time_base, payload);
        p.pts = Some(pts);
        p.dts = Some(pts);
        p.duration = Some(frames as i64);
        p.flags.keyframe = true;
        pts += frames as i64;
        packets.push(p);
    }
    TrackPlan {
        kind: TrackKind::Pcm,
        stream,
        packets,
        ticks_per_sec: rate as i64,
    }
}

fn mjpeg_track(r: &mut Recipe, index: usize, start_delay_ms: i64) -> TrackPlan {
    let width = 16 + (r.u8() % 64) as u32;
    let height = 16 + (r.u8() % 64) as u32;
    let mut params = CodecParameters::video(CodecId::new("mjpeg"));
    params.width = Some(width);
    params.height = Some(height);
    let stream = StreamInfo {
        index: index as u32,
        time_base: TimeBase::new(1, 90_000),
        duration: None,
        start_time: Some(0),
        params,
    };

    let n_pkts = 1 + (r.u8() % 6) as usize;
    let mut packets = Vec::with_capacity(n_pkts);
    // Video media is written at timescale 1000, so keep every 1/90000
    // timestamp a whole multiple of 90 ticks (= 1 ms) for exactness.
    let mut pts = start_delay_ms * 90;
    for i in 0..n_pkts {
        let len = 1 + (r.u16() % 1024) as usize;
        let mut payload = r.take(len);
        payload.resize(len, 0xd8);
        let dur = 90 * (30 + (r.u8() % 12) as i64);
        let mut p = Packet::new(index as u32, stream.time_base, payload);
        p.pts = Some(pts);
        p.dts = Some(pts);
        p.duration = Some(dur);
        p.flags.keyframe = i == 0 || (r.u8() & 1) == 1;
        pts += dur;
        packets.push(p);
    }
    TrackPlan {
        kind: TrackKind::Mjpeg,
        stream,
        packets,
        ticks_per_sec: 90_000,
    }
}

impl MuxPlan {
    pub fn decode(r: &mut Recipe) -> MuxPlan {
        let layout = r.u8() % 4; // 0 plain, 1 faststart, 2 frag, 3 frag+styp+idx
        let fragmented = layout >= 2;
        let brand = match r.u8() % 3 {
            0 => BrandPreset::Mp4,
            1 => BrandPreset::Mov,
            _ => BrandPreset::Ismv,
        };

        // Start delay (leading empty-edit elst) only on non-fragmented
        // layouts, so the pts identity assertion stays sharp. Whole
        // multiples of 10 ms rescale exactly at every supported rate.
        let start_delay_ms = if !fragmented && (r.u8() & 1) == 1 {
            10 * (r.u8() % 200) as i64
        } else {
            0
        };

        let n_tracks = 1 + (r.u8() % 2) as usize;
        let mut tracks = Vec::with_capacity(n_tracks);
        tracks.push(pcm_track(r, 0, start_delay_ms));
        if n_tracks == 2 {
            if r.u8() & 1 == 1 {
                tracks.push(mjpeg_track(r, 1, start_delay_ms));
            } else {
                tracks.push(pcm_track(r, 1, start_delay_ms));
            }
        }

        let mut check_pts = true;
        let frag = if fragmented {
            let cadence = FragmentCadence::EveryNPackets(1 + (r.u8() % 3) as u32);
            let styp = if layout == 3 {
                Some(BrandPreset::Custom {
                    major: *b"msdh",
                    compatible: vec![*b"msdh", *b"msix"],
                })
            } else {
                None
            };
            let options = FragmentedOptions::default()
                .with_cadence(cadence)
                .with_styp(styp)
                .with_emit_random_access_indexes(layout == 3);

            // Optional empty-time insert on track 0 — shifts every
            // later timestamp on that track, so drop the pts check.
            let mut empty_time = Vec::new();
            if r.u8() & 1 == 1 {
                let after = (r.u8() as usize) % tracks[0].packets.len();
                let gap = 1 + (r.u16() % 5_000) as u32;
                empty_time.push((0usize, after, gap));
                check_pts = false;
            }

            let emsg = if r.u8() & 1 == 1 {
                Some(oxideav_mp4::emsg::EmsgBox {
                    scheme_id_uri: "urn:oxideav:fuzz".to_string(),
                    value: "1".to_string(),
                    timescale: 1_000,
                    presentation: if r.u8() & 1 == 1 {
                        oxideav_mp4::emsg::EmsgTime::Absolute(r.u32() as u64)
                    } else {
                        oxideav_mp4::emsg::EmsgTime::Delta(r.u32())
                    },
                    event_duration: r.u32(),
                    id: r.u32(),
                    message_data: r.take(16),
                })
            } else {
                None
            };

            Some(FragPlan {
                options,
                empty_time,
                emsg,
            })
        } else {
            None
        };

        let options = Mp4MuxerOptions::default()
            // fragmented goes through open_fragmented_typed
            .with_brand(brand)
            .with_faststart(layout == 1)
            .with_fragmented(None);

        MuxPlan {
            tracks,
            options,
            frag,
            check_pts,
        }
    }

    /// Execute the plan through our own muxer. A muxer error on a plan
    /// this module constructed is a contract violation → panic.
    pub fn run(&self) -> Built {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let ws: Box<dyn WriteSeek> = Box::new(SharedBuf {
            buf: Arc::clone(&buf),
            pos: 0,
        });
        let streams: Vec<StreamInfo> = self.tracks.iter().map(|t| t.stream.clone()).collect();

        // Interleave packets across tracks in pts order (stable on
        // ties → track order), the write order a real packager uses.
        let mut order: Vec<(usize, usize)> = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            for pi in 0..t.packets.len() {
                order.push((ti, pi));
            }
        }
        order.sort_by_key(|&(ti, pi)| {
            let p = &self.tracks[ti].packets[pi];
            // Rescale to a shared µs scale for cross-track ordering.
            let tps = self.tracks[ti].ticks_per_sec.max(1);
            (p.pts.unwrap_or(0) * 1_000_000 / tps, ti, pi)
        });

        if let Some(frag) = &self.frag {
            let mut mux = oxideav_mp4::frag::open_fragmented_typed(
                ws,
                &streams,
                self.options.clone(),
                frag.options.clone(),
            )
            .expect("fragmented muxer open on a valid-by-construction plan");
            mux.write_header().expect("fragmented write_header");
            if let Some(e) = &frag.emsg {
                mux.set_next_segment_emsg(std::iter::once(e.clone()));
            }
            for &(ti, pi) in &order {
                mux.write_packet(&self.tracks[ti].packets[pi])
                    .expect("fragmented write_packet");
                for &(gt, gafter, gdur) in &frag.empty_time {
                    if gt == ti && gafter == pi {
                        mux.insert_empty_time(gt, gdur).expect("insert_empty_time");
                    }
                }
            }
            mux.write_trailer().expect("fragmented write_trailer");
        } else {
            let mut mux = oxideav_mp4::muxer::open_with_options(ws, &streams, self.options.clone())
                .expect("muxer open on a valid-by-construction plan");
            mux.write_header().expect("write_header");
            for &(ti, pi) in &order {
                mux.write_packet(&self.tracks[ti].packets[pi])
                    .expect("write_packet");
            }
            mux.write_trailer().expect("write_trailer");
        }

        let bytes = buf.lock().unwrap().clone();
        let tracks = self
            .tracks
            .iter()
            .map(|t| TrackExpect {
                payloads: t.packets.iter().map(|p| p.data.clone()).collect(),
                pts: t.packets.iter().map(|p| p.pts.unwrap_or(0)).collect(),
                tb_den: t.ticks_per_sec,
            })
            .collect();
        Built {
            bytes,
            tracks,
            check_pts: self.check_pts,
        }
    }

    pub fn n_tracks(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_fragmented(&self) -> bool {
        self.frag.is_some()
    }

    /// Whether track `i` is the arbitrary-payload video kind.
    pub fn is_video(&self, i: usize) -> bool {
        matches!(self.tracks[i].kind, TrackKind::Mjpeg)
    }
}

// ─────────────────────── Demux exercise battery ───────────────────────

/// Bound on how many packets we drain per input (see the demux target).
pub const MAX_PACKETS_PER_INPUT: usize = 256;

/// Feed `data` through the typed open front and, when it opens, the
/// full public accessor battery + bounded drain + both seek paths.
pub fn exercise_demux(data: &[u8]) {
    if data.len() < 8 {
        return;
    }
    let rs: Box<dyn ReadSeek> = Box::new(Cursor::new(data.to_vec()));
    let Ok(mut dmx) = oxideav_mp4::demux::open_typed(rs, &NullCodecResolver) else {
        return;
    };

    // Post-open invariants: every accessor must be callable on
    // whatever state the parser left behind.
    let n_streams = dmx.streams().len();
    let _ = dmx.metadata().len();
    let _ = dmx.duration_micros();
    let _ = dmx.empty_duration_records().len();
    let _ = dmx.psshes().len();
    let _ = dmx.moof_psshes().len();
    let _ = dmx.senc_records().len();
    let _ = dmx.pdin_entries().map(|e| e.len());
    let _ = dmx.pnot().is_some();
    let _ = dmx.leva_entries().map(|e| e.len());
    let _ = dmx.treps().len();
    let _ = dmx.ssixes().len();
    let _ = dmx.sai_records().len();
    let _ = dmx.traf_sample_groups().len();
    let _ = dmx.piff_psshes().len();
    let _ = dmx.piff_moof_psshes().len();
    let _ = dmx.piff_senc_records().len();
    let _ = dmx.piff_tencs().len();
    let _ = dmx.meco();

    for i in 0..n_streams as u32 {
        let _ = dmx.edit_list(i).len();
    }

    // emsg records + the v0 delta→absolute anchoring math.
    for k in 0..dmx.emsgs().len() {
        let rec = &dmx.emsgs()[k];
        let _ = dmx.emsg_absolute_time(rec);
    }

    // HEIF item catalogue: byte-range resolution + idat materialisation
    // for every item the iloc mentions (plus the primary item), and the
    // property associations for each — cycles / out-of-range indexes
    // must resolve or fail without panicking.
    {
        let meta = dmx.meta_items();
        let mut ids: Vec<u32> = Vec::new();
        if let Some(iloc) = &meta.iloc {
            ids.extend(iloc.items.iter().map(|it| it.item_id));
        }
        if let Some(pid) = meta.primary_item_id {
            ids.push(pid);
        }
        for id in ids {
            let _ = meta.item_byte_ranges(id);
            let _ = meta.item_data_from_idat(id);
            if let Some(iprp) = &meta.iprp {
                let _ = iprp.properties_for(id).len();
            }
        }
    }

    // CENC aux-info carriage (saiz/saio) — seeks + bounded allocations.
    let _ = dmx.resolve_sai_aux_info();

    // Bounded drain.
    for _ in 0..MAX_PACKETS_PER_INPUT {
        if dmx.next_packet().is_err() {
            break;
        }
    }
    let _ = dmx.sample_description_index_of_last_packet();

    // Both seek paths: sync-sample landing at 0 and at a mid pts, then
    // a short re-drain from the seeked position.
    let _ = dmx.seek_to(0, 0);
    let _ = dmx.seek_to(0, 1_000);
    for _ in 0..8 {
        if dmx.next_packet().is_err() {
            break;
        }
    }
}
