# seed-soak fleet report (1783567881)

- config: 3 masters + 25 viewers, corpus 586 files / 0.47 GB, duration 3600s, interval 30s
- scenarios: churn=Some(300) degrade_viewer=Some(0) conflict=false health_secs=Some("600/900")
- verdict: **PASS with anomalies (see timeline)**
- all nodes Healthy at end: true; swarm-deadline log hits: 0

## Convergence verification

- node-00: byte-identical ✓
- node-01: byte-identical ✓
- node-02: byte-identical ✓
- node-03: byte-identical ✓
- node-04: byte-identical ✓
- node-05: byte-identical ✓
- node-06: byte-identical ✓
- node-07: byte-identical ✓
- node-08: byte-identical ✓
- node-09: byte-identical ✓
- node-10: byte-identical ✓
- node-11: byte-identical ✓
- node-12: byte-identical ✓
- node-13: byte-identical ✓
- node-14: byte-identical ✓
- node-15: byte-identical ✓
- node-16: byte-identical ✓
- node-17: byte-identical ✓
- node-18: byte-identical ✓
- node-19: byte-identical ✓
- node-20: byte-identical ✓
- node-21: byte-identical ✓
- node-22: byte-identical ✓
- node-23: byte-identical ✓
- node-24: byte-identical ✓
- node-25: byte-identical ✓
- node-26: byte-identical ✓
- node-27: byte-identical ✓

## PeerHealth events (observed on node-00)

- t+907s  UNHEALTHY  viewer-03  100%  600s  self=false
- t+1465s  UNHEALTHY    100%  600s  self=true
- t+1807s  RECOVERED  viewer-03  100%  0s  self=false
- t+1813s  RECOVERED    100%  0s  self=true

## Anomaly timeline

- t+781s node-22 OutOfSync sustained > 5 min
- t+841s node-04 OutOfSync sustained > 5 min
- t+841s node-07 OutOfSync sustained > 5 min
- t+871s node-12 OutOfSync sustained > 5 min
- t+871s node-13 OutOfSync sustained > 5 min
- t+901s node-15 OutOfSync sustained > 5 min
- t+931s node-19 OutOfSync sustained > 5 min
- t+991s node-20 OutOfSync sustained > 5 min
- t+1021s node-11 OutOfSync sustained > 5 min
- t+1051s node-10 OutOfSync sustained > 5 min
- t+1051s node-16 OutOfSync sustained > 5 min
- t+1472s node-10 OutOfSync sustained > 5 min
- t+1472s node-25 OutOfSync sustained > 5 min
- t+1502s node-18 OutOfSync sustained > 5 min
- t+1532s node-09 OutOfSync sustained > 5 min
- t+1562s node-07 OutOfSync sustained > 5 min
- t+1562s node-08 OutOfSync sustained > 5 min
- t+1592s node-19 OutOfSync sustained > 5 min
- t+1652s node-05 OutOfSync sustained > 5 min
- t+1712s node-04 OutOfSync sustained > 5 min
- t+1712s node-14 OutOfSync sustained > 5 min

Samples: `samples.csv` next to this report. Per-daemon logs under `node-NN/daemon.log`.
