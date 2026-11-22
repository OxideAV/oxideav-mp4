#!/bin/sh
# Regenerates the fixtures in this directory with ffmpeg (provenance of
# the files; the tests read them as they are committed). Each one pins a
# demuxer behaviour checked against what ffprobe reports for the file.
set -e
cd "$(dirname "$0")"
V="-f lavfi -i testsrc2=s=32x32:r=25:d=0.24"
A="-f lavfi -i sine=f=440:d=0.24"
X="-v error -y -bitexact"
# Two track fragments a movie fragment, no base data offset and no
# default-base-is-moof: the second one's data follows the first one's.
ffmpeg $X $V $A -c:v libx264 -preset ultrafast -bf 0 -g 3 -c:a aac -b:a 16k \
  -movflags frag_keyframe+empty_moov+omit_tfhd_offset frag-inherited-base.mp4
# Opus: the dOps box against the identification header.
ffmpeg $X -f lavfi -i sine=f=440:d=0.1:r=48000 -c:a libopus -b:a 16k opus.mp4
# Rates the 16.16 samplerate field cannot hold.
ffmpeg $X -f lavfi -i sine=f=440:d=0.1:r=96000 -c:a aac -b:a 32k aac-96k.mp4
ffmpeg $X -f lavfi -i sine=f=440:d=0.02:r=96000 -sample_fmt s32 -c:a flac -strict -2 flac-96k-32bit.mp4
# Uncompressed audio: one table sample per audio frame.
ffmpeg $X -f lavfi -i sine=f=440:d=0.025:r=48000 -c:a pcm_s16be pcm-s16be.mp4
ffmpeg $X -f lavfi -i sine=f=440:d=0.025:r=48000 -c:a pcm_s24le pcm-s24le.mov
# Smooth Streaming: media header duration unknown, negative
# composition offsets.
ffmpeg $X $V -c:v libx264 -preset ultrafast -bf 2 -g 6 smooth.ismv
# B-frames without an edit list: the first presented time is not zero.
ffmpeg $X $V -c:v libx264 -preset ultrafast -bf 2 -g 6 -use_editlist 0 no-edit-list.mp4
