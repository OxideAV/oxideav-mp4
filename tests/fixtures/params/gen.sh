# Black-box generator for the stream-parameter fixtures (ffmpeg 9.0.1 + libx264 /
# libx265 / libvpx-vp9 / libsvtav1). Reference values: ffprobe -show_streams.
set -e
S="-f lavfi -i testsrc2=size=32x18:rate=25 -frames:v 2 -an -map_metadata -1 -fflags +bitexact"
for pf in yuv420p yuv420p10le yuv422p yuv444p gray; do ffmpeg -y -v error $S -c:v libx264 -preset ultrafast -pix_fmt $pf h264-$pf.mp4; done
for pf in yuv420p yuv420p10le yuv422p10le yuv444p gray; do ffmpeg -y -v error $S -c:v libx265 -preset ultrafast -x265-params log-level=none -pix_fmt $pf hevc-$pf.mp4; done
for pf in yuv420p yuv420p10le yuv422p yuv444p yuv440p; do ffmpeg -y -v error $S -c:v libvpx-vp9 -deadline realtime -pix_fmt $pf vp9-$pf.mp4; done
for pf in yuv420p yuv420p10le; do ffmpeg -y -v quiet -f lavfi -i testsrc2=size=64x64:rate=25 -frames:v 2 -an -map_metadata -1 -fflags +bitexact -c:v libsvtav1 -preset 12 -pix_fmt $pf av1-$pf.mp4; done
ffmpeg -y -v error -f lavfi -i testsrc2=size=32x18:rate=30000/1001 -frames:v 3 -an -map_metadata -1 -fflags +bitexact -c:v libx264 -preset ultrafast -pix_fmt yuv420p h264-ntsc.mp4
