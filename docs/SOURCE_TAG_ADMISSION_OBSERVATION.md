# Source tag preparation observations

`SourceWriter::capture_tag_admission_observation` produces an opaque, read-only
input for metadata-only journal preparation. It combines the existing complete
normal-tag assignment selection, the owner's measured capped SQLite allocation
budget, the active directory generation, and a matching no-follow database
handle opened through the retained profile child root. Selection and generation
are read in one SQLite transaction. Capture validates source authority and the
budget before returning; successful commands must finish within two seconds.
SQLite statement progress guards bound database work and reject overruns.
The process extension-free guard spans the entire capture/revalidation sequence,
so initialization cannot run between its measured budget and selection checks.

Rename retains at most 64 paths; delete retains at most 128. These limits reject
the entire request when exceeded. Accepted paths are sorted in Rust after the
bounded read, avoiding SQLite temporary sorting before capacity admission and
an entered memory-only policy. Existing canonical label, classification,
collision, supported-path and manifest checks continue to apply.

`validate_tag_admission_observation` runs on the original retained SourceWriter.
It checks issuer liveness, the current physical source lease, matching retained
file identity/length, all measured budget/schema facts, the exact source revision
and the complete selection and directory generation. It checks the original
budget again after recapture. An owner handoff invalidates the observation even
when the durable source revision has not changed. Allocation changes may also
invalidate an observation after a logical state is restored; callers must obtain
a fresh observation rather than reuse stale measurements.

The journal's private capacity planner can borrow the observed descriptor after
physical liveness validation. That borrow alone does not check logical selection
freshness. Admission must route complete revalidation through the source owner;
it cannot treat a historical serialized request or this descriptor as authority.

This API allocates no OperationId, writes no intent or receipt, reserves no
capacity and authorizes no assignment effect. OPT-2099 still owns versioned
metadata-only journal admission, persistent claims and the private admission
proof. OPT-2049 owns the admitted assignment transaction and immutable V2
receipt. Profile-wide definition finalization and native physical qualification
remain separate outstanding scope.

Linux regressions use actual profile-bound SourceWriter and SQLite fixtures.
They verify successful capture without source writes or receipt changes;
rejection of corrupted directory publication/counts, changed membership or
complete generation, unknown schema, changed allocation policy, changed source
revision and owner handoff. Existing bounded selection, budget and policy
regressions remain required. These checks do not qualify native storage effects.
