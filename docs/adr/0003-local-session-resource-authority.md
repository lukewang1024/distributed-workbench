# Permanent session homes and Executor resource arbitration

Sessions belong permanently to their creating Controller. Calls default to local scope and only an explicit sessionControllerId routes to another Controller. Legacy foreign records remain readable through explicit references; session handoff is retired without deleting historical state. Profiles are configuration templates rather than session identities.

Each Executor arbitrates its resource leases and capability locks across all incoming Controllers. Controller driver leases coordinate Agents in one session, while Executor resource grants protect physical resources. An in-flight operation keeps its resource busy even after its lease expires; restart with an uncertain operation quarantines that resource. This avoids copying lease state or treating independent Controller fence counters as a global ordering.
