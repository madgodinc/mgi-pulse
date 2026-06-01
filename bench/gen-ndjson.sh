#!/usr/bin/env bash
# Synthetic NDJSON generator for mgi-pulse M1 parse-cost benchmark.
# Emits N lines of typical Aurora-style logs to stdout. ~232 B/line on average.
# Usage: gen-ndjson.sh <N> > out.ndjson
set -euo pipefail

N="${1:?lines}"

# Single-pass awk for speed. Pure bash printf is ~50x slower for 10M lines.
base_s=$(date +%s)

awk -v N="$N" -v BASE="$base_s" 'BEGIN{
  srand(20260601);
  nlv = split("trace debug info info info info warn warn error", lv, " ");
  nlg = split("aurora.tts aurora.stt aurora.llm aurora.dispatch aurora.session aurora.metrics", lg, " ");
  # messages with spaces — split on |
  nms = split("synthesized chunk in 87ms|vad gate closed after 312ms silence|prompt eval finished, 142 tok in 91ms|ws dispatch ok|stream session opened|metrics flushed to clickhouse|queue depth above watermark|downstream timeout, retrying|fatal: model unloaded mid-stream", ms, "|");

  base_us = BASE * 1000000;
  for (i = 0; i < N; i++) {
    ts_us = base_us + i * 137 + int(rand() * 1000);
    s  = int(ts_us / 1000000);
    us = ts_us % 1000000;
    # gmtime via strftime (awk uses local TZ — for bench it does not matter,
    # mgi-pulse parses RFC3339 with timezone offset). Use UTC by forcing TZ.
    ts_str = strftime("%Y-%m-%dT%H:%M:%S", s, 1);

    L = lv[int(rand()*nlv)+1];
    G = lg[int(rand()*nlg)+1];
    M = ms[int(rand()*nms)+1];
    rid_hi = sprintf("%016x", ts_us);
    rid_lo = sprintf("%04x", int(rand()*65536));
    chars  = int(rand()*400);
    lat    = int(rand()*500);

    printf("{\"ts\":\"%s.%06dZ\",\"level\":\"%s\",\"logger\":\"%s\",\"msg\":\"%s\",\"request_id\":\"req-%s-%s\",\"payload\":{\"chars\":%d,\"latency_ms\":%d}}\n",
      ts_str, us, L, G, M, rid_hi, rid_lo, chars, lat);
  }
}'
