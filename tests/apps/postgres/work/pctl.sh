#!/usr/bin/env bash
# Latency percentiles (ms) from pgbench -l logs: pctl.sh PREFIX
cat "${1:?prefix}"* 2>/dev/null | awk '$3 ~ /^[0-9]+$/ {print $3}' | sort -n |
    awk '{a[NR]=$1} END {if (NR==0) {print "no data"; exit}
        printf "n=%d p50=%.2f p95=%.2f p99=%.2f p99.9=%.2f max=%.2f ms\n", NR,
        a[int(NR*0.5)+1]/1000, a[int(NR*0.95)+1]/1000, a[int(NR*0.99)+1]/1000,
        a[int(NR*0.999)+1]/1000, a[NR]/1000}'
