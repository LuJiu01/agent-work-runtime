# Team V1 known limits

Automated PostgreSQL tests cover protocol behavior for the 69 required
scenarios. They are not a substitute for a production SLA or multi-region HA.

A live dual-client run used **Kimi Code CLI 2.0.0** and **ZCode CLI 0.16.5**
on one dedicated Team project (`p11-live` / `work-p11`): claim, implementation,
handoff, verification, human review, completion. An independent oracle checked
one receipt, the successor holder, and a single execution. That run is recorded
in the evidence matrix as `live_agent_run`. It does not replay all 69 cases as
Agent sessions and does not authorize a release tag.

Capacity probes record actual local rates. They must not be quoted as
enterprise throughput.
