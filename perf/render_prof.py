"""Summarise herdr's render profiler (HERDR_RENDER_PROF=1) from a log excerpt.

Reads server log lines on stdin; prints "<full renders per second> <mean
full_render.total in ms>" over WINDOW seconds (argv[1]). The mean is
"-" when nothing was fully rendered, which is a build doing all its drawing
with retained patches, not a free render.
"""
import re
import sys

window = float(sys.argv[1])
renders = 0
virt_count = 0
virt_total_us = 0
for line in sys.stdin:
    if 'event="render.prof"' not in line:
        continue
    m = re.search(r"full_render\.invoke=(\d+)", line)
    if m:
        renders += int(m.group(1))
    m = re.search(r"full_render\.total=count:(\d+) avg_us:(\d+)", line)
    if m:
        virt_count += int(m.group(1))
        virt_total_us += int(m.group(1)) * int(m.group(2))
avg = f"{virt_total_us / virt_count / 1000:.1f}" if virt_count else "-"
print(f"{renders / window:.1f} {avg}")
