# Release identity belongs to each component

Distributed Workbench owns the host adapter, generic immutable installation engine and
Fabric reconciliation. Composition owns package membership, exact version selection,
credentials, channel promotion and application launch/resource policy. Keeping one
implementation of installation transactions avoids private/public recovery behavior drifting.

The Computer Use host is a small platform-neutral component from this repository. Its
manifest pins the transport protocol, Node version and dependency lock digest. Existing
sessions retain an immutable host process; a new host selection applies at the next
host start. Dependencies stay in the existing platform runtime. A dependency/protocol
change still requires a compatible runtime/core release. Rollback selects a previously
verified host and never replays GUI actions.

The updater library accepts explicit composition policy and storage paths. Its default
release contract has a package identity and permits one or more platform artifacts;
private compatibility schemas and historical paths belong to the private wrapper.
Rollout nodes list their components and preserve declared order. Legacy role plans are
accepted for migration. A locally healthy but fabric-degraded upgrade pauses by default;
rollback is an explicit plan policy. Continuing requires fabric recovery first.
