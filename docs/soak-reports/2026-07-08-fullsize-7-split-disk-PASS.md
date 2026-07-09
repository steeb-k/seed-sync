# seed-soak fullsize report (1783551757)

- config: 3 masters + 3 viewers, corpus 4006 files / 41.94 GB, duration 5400s, interval 30s
- scenarios: churn=None degrade_viewer=Some(0) conflict=false health_secs=Some("600/900")
- split roots: nodes 4..5 under D:\seed-soak-alt (rest under C:\Users\steeb-ai\seed-soak-full)
- verdict: **PASS with anomalies (see timeline)**
- all nodes Healthy at end: true; swarm-deadline log hits: 0

## Convergence verification

- node-00: byte-identical ✓
- node-01: byte-identical ✓
- node-02: byte-identical ✓
- node-03: byte-identical ✓
- node-04: byte-identical ✓
- node-05: byte-identical ✓

## PeerHealth events (observed on node-00)

- t+594s  UNHEALTHY  master-02  97%  600s  self=false
- t+606s  RECOVERED  master-02  100%  0s  self=false
- t+618s  UNHEALTHY  viewer-05  4%  600s  self=false
- t+624s  UNHEALTHY  viewer-03  60%  600s  self=false
- t+678s  UNHEALTHY  viewer-04  4%  600s  self=false
- t+1518s  UNHEALTHY  viewer-05  8%  1416s  self=false
- t+1524s  UNHEALTHY  viewer-03  60%  1500s  self=false
- t+1578s  UNHEALTHY  viewer-04  33%  1464s  self=false
- t+2418s  UNHEALTHY  viewer-05  8%  2226s  self=false
- t+2424s  UNHEALTHY  viewer-03  60%  2400s  self=false
- t+2478s  UNHEALTHY  viewer-04  54%  2058s  self=false
- t+2760s  RECOVERED  viewer-03  100%  0s  self=false
- t+3318s  UNHEALTHY  viewer-05  40%  2922s  self=false
- t+4218s  UNHEALTHY  viewer-05  40%  3810s  self=false
- t+4374s  RECOVERED  viewer-05  100%  0s  self=false

## Anomaly timeline

- t+304s node-05 IPC failed (daemon dead?): IPC request timed out (15s)
- t+380s node-00 CPU > 25% sustained > 5 min (203%)
- t+380s node-01 CPU > 25% sustained > 5 min (267%)
- t+380s node-02 CPU > 25% sustained > 5 min (182%)
- t+380s node-03 CPU > 25% sustained > 5 min (76%)
- t+501s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+557s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+633s node-05 IPC failed (daemon dead?): IPC request timed out (15s)
- t+678s node-05 IPC failed (daemon dead?): IPC request timed out (15s)
- t+1212s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+1577s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+1577s node-05 IPC failed (daemon dead?): IPC request timed out (15s)
- t+1881s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+1934s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+1980s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+2026s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+2085s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+2434s node-05 IPC failed (daemon dead?): IPC request timed out (15s)
- t+2479s node-05 IPC failed (daemon dead?): IPC request timed out (15s)
- t+2524s node-05 IPC failed (daemon dead?): IPC request timed out (15s)
- t+3048s node-03 CPU > 25% sustained > 5 min (51%)
- t+3549s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+3594s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+3639s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+3685s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+3732s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+4267s node-05 IPC failed (daemon dead?): IPC request timed out (15s)
- t+4312s node-05 IPC failed (daemon dead?): IPC request timed out (15s)
- t+5094s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+5140s node-04 IPC failed (daemon dead?): IPC request timed out (15s)
- t+5230s node-05 IPC failed (daemon dead?): IPC request timed out (15s)

Samples: `samples.csv` next to this report. Per-daemon logs under `node-NN/daemon.log`.
