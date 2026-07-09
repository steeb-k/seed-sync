# seed-soak midsize report (1783558324)

- config: 1 masters + 2 viewers, corpus 355 files / 4.19 GB, duration 900s, interval 30s
- scenarios: churn=None degrade_viewer=None conflict=false health_secs=Some("600/900")
- split roots: nodes 2..2 under D:\seed-soak-alt (rest under C:\Users\steeb-ai\seed-soak-mid)
- verdict: **PASS**
- all nodes Healthy at end: true; swarm-deadline log hits: 0

## Convergence verification

- node-00: byte-identical ✓
- node-01: byte-identical ✓
- node-02: byte-identical ✓

## PeerHealth events (observed on node-00)


## Anomaly timeline

- none

Samples: `samples.csv` next to this report. Per-daemon logs under `node-NN/daemon.log`.
