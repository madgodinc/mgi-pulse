#!/usr/bin/env bash
# Bursty variant of gen-ndjson.sh — time-varying severity distribution so
# the timeline histogram has visible structure (peaks of errors, lulls of
# debug, etc.) instead of being a uniform flat strip. Used for the README
# hero screenshots; for benchmarking parse cost, use gen-ndjson.sh (the
# uniform version) so timing isn't biased by skewed severity.
#
# Layout of the 11 M-record / ~25 min synthetic timeline this produces:
#   t = 0%–15%   : calm — mostly info / debug, scattered warn
#   t = 15%–25%  : warn ramp — gradual warn-spike (degraded health)
#   t = 25%–35%  : error burst — incident, ~60% errors, warns still high
#   t = 35%–50%  : recovery — error tail-off, info dominates
#   t = 50%–65%  : steady state — even distribution, light trace activity
#   t = 65%–75%  : second smaller error spike — flaky retry storm
#   t = 75%–90%  : debug-heavy — verbose retry traces (high debug ratio)
#   t = 90%–100% : tail calm — back to nominal
#
# Same line shape as gen-ndjson.sh, so anything that parsed the uniform
# fixture parses this one identically. Same ~232 B/line average.
#
# Usage: gen-ndjson-bursty.sh <N> > out.ndjson
set -euo pipefail

N="${1:?lines}"

base_s=$(date +%s)

awk -v N="$N" -v BASE="$base_s" 'BEGIN{
  srand(20260601);
  nlg = split("aurora.tts aurora.stt aurora.llm aurora.dispatch aurora.session aurora.metrics", lg, " ");
  nms = split("synthesized chunk in 87ms|vad gate closed after 312ms silence|prompt eval finished, 142 tok in 91ms|ws dispatch ok|stream session opened|metrics flushed to clickhouse|queue depth above watermark|downstream timeout, retrying|fatal: model unloaded mid-stream", ms, "|");

  # Severity tables, one per phase. Each is an array of severity strings
  # whose lengths encode the per-phase distribution. Listing "error" five
  # times in a 10-slot table means a 50% error rate in that phase.
  nc1 = split("info info info info info info debug debug debug warn", calm, " ");
  nw1 = split("info info info warn warn warn warn debug error info", warmup, " ");
  ne1 = split("error error error error error error warn warn debug fatal", incident, " ");
  nr1 = split("info info info info info debug debug warn error info", recovery, " ");
  ns1 = split("info info debug debug warn warn trace error info debug", steady, " ");
  ne2 = split("error error error warn warn warn warn debug info info", flaky, " ");
  nd1 = split("debug debug debug debug debug trace trace info warn error", debugheavy, " ");
  nt1 = split("info info info info debug debug info warn info trace", tailcalm, " ");

  base_us = BASE * 1000000;
  for (i = 0; i < N; i++) {
    ts_us = base_us + i * 137 + int(rand() * 1000);
    s  = int(ts_us / 1000000);
    us = ts_us % 1000000;
    ts_str = strftime("%Y-%m-%dT%H:%M:%S", s, 1);

    # Phase by progress through the file.
    p = i / N;
    if      (p < 0.15) { L = calm[int(rand()*nc1)+1]; }
    else if (p < 0.25) { L = warmup[int(rand()*nw1)+1]; }
    else if (p < 0.35) { L = incident[int(rand()*ne1)+1]; }
    else if (p < 0.50) { L = recovery[int(rand()*nr1)+1]; }
    else if (p < 0.65) { L = steady[int(rand()*ns1)+1]; }
    else if (p < 0.75) { L = flaky[int(rand()*ne2)+1]; }
    else if (p < 0.90) { L = debugheavy[int(rand()*nd1)+1]; }
    else               { L = tailcalm[int(rand()*nt1)+1]; }

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
