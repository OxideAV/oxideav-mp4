//! Pure-Rust MP4 / ISO Base Media File Format container.
//!
//! Scope: demuxer for probe + remux of audio and video tracks, plus a
//! moov-at-end muxer with optional faststart (moov-at-front) rewrite.
//! Three brand presets are registered: `mp4`, `mov`, and `ismv` — all
//! share one implementation and only differ in their `ftyp` preset.

pub mod boxes;
pub mod codec_id;
pub mod demux;
pub(crate) mod frag;
pub mod muxer;
pub mod options;
mod sample_entries;

pub use options::{BrandPreset, FragmentCadence, FragmentedOptions, Mp4MuxerOptions};

use oxideav_core::ContainerRegistry;

pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("mp4", demux::open);
    reg.register_muxer("mp4", muxer::open);
    reg.register_muxer("mov", muxer::open_mov);
    reg.register_muxer("ismv", muxer::open_ismv);
    // Fragmented MP4: emit init-segment (ftyp+moov+mvex) then per-fragment
    // styp+moof+mdat. Default cadence: every 2 seconds (see
    // FragmentedOptions::default). Suitable for DASH / HLS / CMAF output
    // when piped through a segment slicer.
    reg.register_muxer("dash", muxer::open_dash);
    reg.register_muxer("cmaf", muxer::open_dash);
    reg.register_extension("mp4", "mp4");
    reg.register_extension("m4a", "mp4");
    reg.register_extension("m4v", "mp4");
    reg.register_extension("mov", "mov");
    reg.register_extension("3gp", "mp4");
    reg.register_extension("ismv", "ismv");
    reg.register_extension("m4s", "dash");
    reg.register_probe("mp4", probe);
}

/// Install the MP4 / MOV / ISMV / DASH / CMAF containers into a
/// [`oxideav_core::RuntimeContext`].
///
/// Convenience wrapper around [`register_containers`] that matches the
/// uniform `register(&mut RuntimeContext)` entry point every sibling
/// crate exposes.
///
/// Also wired into [`oxideav_meta::register_all`] via the
/// [`oxideav_core::register!`] macro below.
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("mp4", register);

/// `....ftyp` at offset 0 — ISO base media file format. Some files lead
/// with a `wide` or `free` box before `ftyp`, so accept that with a
/// slightly lower confidence.
fn probe(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() < 8 {
        return 0;
    }
    if &p.buf[4..8] == b"ftyp" {
        return 100;
    }
    if p.buf.len() >= 16
        && matches!(&p.buf[4..8], b"wide" | b"free" | b"skip")
        && &p.buf[12..16] == b"ftyp"
    {
        return 90;
    }
    // QuickTime sometimes writes `moov` first, no `ftyp`.
    if &p.buf[4..8] == b"moov" {
        return 50;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_via_runtime_context_installs_container() {
        let mut ctx = oxideav_core::RuntimeContext::new();
        register(&mut ctx);
        assert_eq!(ctx.containers.container_for_extension("mp4"), Some("mp4"));
        assert_eq!(ctx.containers.container_for_extension("mov"), Some("mov"));
        assert_eq!(ctx.containers.container_for_extension("m4s"), Some("dash"));
    }
}
