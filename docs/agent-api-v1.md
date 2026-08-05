# Pithos Agent API v1

Status: implemented for Pithos R1 / PAF 0.1-draft.

## Transport and framing

`pithosd` exposes JSON-RPC 2.0 only through local IPC:

- Windows: byte-mode Named Pipe with remote clients rejected and a first-instance lock;
- Linux/macOS: Unix Domain Socket in a `0700` state directory, socket mode `0600`, with peer UID validation.

No TCP listener exists. Each message is framed as a four-byte little-endian unsigned payload length followed by exactly that many UTF-8 JSON bytes. The default maximum frame is 1 MiB and incomplete frame reads time out after ten seconds. Length is validated before allocation. Batch requests and JSON-RPC notifications are not part of Agent API v1.

## Session handshake

The first request on a connection is `capabilities`. It declares the client protocol version and requested read/write roots. The response contains a 256-bit random capability token bound to that connection, an opaque session ID and a separate 256-bit resume token. Every later request on that connection must provide the capability token.

The daemon validates both the session scope and the narrower path scope carried by each request. Jobs, status and events belonging to another session are reported as `job_not_found`, without revealing their existence. Tokens are never persisted or logged.

The preceding statement applies literally to connection capability tokens. For restart-safe job ownership, the daemon persists only the BLAKE3 hash of each resume token together with the durable session ID, original canonical scope and expiry. A reconnect sends `resume: {session_id, resume_token}` in `capabilities`; the daemon keeps the same session identity, creates a fresh connection capability, rotates the resume token, and rechecks the stored scope against the current server allowlist. The old resume token becomes invalid immediately. Sessions expire after eight hours by default and expired records are pruned. The raw resume secret is never written to disk or returned by the CLI `capabilities` output.

The security boundary is the local OS user: socket/pipe permissions prevent access by remote or other ordinary users. A capability token isolates connections belonging to that user; it is not intended to defend against a hostile process already running with the same OS identity. Path scopes canonicalize and intersect server/request roots, reject symlink escapes, and are revalidated immediately before execution. They are a client-scope safety barrier, not a handle-relative capability filesystem against a same-identity process racing namespace changes between validation and I/O. Deployments with mutually hostile local processes must run the daemon under a dedicated OS account whose filesystem permissions contain it to the allowed roots.

## Methods

| Method | Result |
|---|---|
| `capabilities` | versions, methods, codecs, profiles, limits and connection capability |
| `estimate` | bounded scan estimate; never promises a final compression ratio |
| `pack` | asynchronous `JobAccepted` |
| `unpack` | asynchronous `JobAccepted` |
| `list` | asynchronous `JobAccepted`; metadata-only catalog read |
| `inspect` | asynchronous `JobAccepted`; metadata-only catalog read |
| `extract` | asynchronous `JobAccepted`; selected group only |
| `read_range` | asynchronous `JobAccepted`; verified local transfer file, never base64 |
| `verify` | asynchronous `JobAccepted`; full CRC32C/BLAKE3 validation |
| `cancel` | current cancellation state |
| `job_status` | session-owned persistent job snapshot |
| `subscribe_events` | bounded long-poll replay after a sequence number |

Method parameters are strict schemas: unknown fields, wrong types, invalid identifiers, non-integer request IDs, methods outside the allowlist, excessive nesting, oversized arrays/strings and malformed JSON are rejected before a job is created. There is no shell or raw-command method.

## Compression profiles

`estimate` and `pack` accept the strict JSON enum `raw`, `stream`, `random`,
`balanced` or `archive_max`. Omitting the field defaults to `raw` for backward
compatibility; any other value is rejected before work is created. The CLI
spelling of the last value is `archive-max`, and `capabilities` reports the same
hyphenated user-facing spelling.

| Agent API value | Solid-group target | Candidate codecs |
|---|---:|---|
| `raw` | independent files | STORE |
| `stream` | 4 MiB | STORE, Zstandard |
| `random` | 8 MiB | STORE, Zstandard |
| `balanced` | 64 MiB | STORE, Zstandard, Brotli, LZMA2 |
| `archive_max` | 512 MiB | STORE, Zstandard, Brotli, LZMA2 |

Candidate selection accounts for payload and structural costs and uses a stable
codec-ID tie-break. Resource estimates are profile-aware, include conservative
container overhead and use checked arithmetic. They remain estimates rather than
promises of a compression ratio.

## Jobs, idempotency and events

Public states are `queued`, `running`, `cancelling`, `completed`, `failed` and `cancelled`. Terminal state is immutable. A queued cancellation publishes no output. A running cancellation signals the engine token and waits for an I/O checkpoint. If atomic publication has already won the race, the job completes instead of claiming that a published artifact was cancelled.

`idempotency_key` is scoped to a session. The daemon hashes the validated operation, canonical paths and job limits. Reusing the key with the same hash returns the original `JobId`; reusing it with different parameters returns `job_conflict`. Completed job records and their idempotency mappings are not evicted. At the bounded retention capacity, existing keys still replay or conflict normally, while a new unique key is rejected with `resource_limit` instead of silently forgetting prior work.

Every persisted transition appends an event in the same atomic snapshot. Event sequence starts at one per job and increases monotonically. `subscribe_events` accepts `after_sequence` and an optional bounded `wait_ms`; slow clients therefore retain cursors instead of creating an unbounded server queue.

Accepted work enters a weighted, session-fair priority queue. Interactive reads/extracts can overtake queued background work, while weighted round-robin service prevents a high-priority or single-session stream from permanently starving other lanes. A deadline starts when the job is accepted, so it also applies while a job waits for execution resources.

## Persistence and restart

Session and job state are encoded in separate versioned JSON snapshots. The private state directory is secured before either store is opened. Updates are written to a random temporary file in that directory, synchronized and atomically persisted. Live memory is aligned with an already-published rename even if the following parent-directory durability barrier reports an error. Mutations are serialized so concurrent reuse of an idempotency key can create at most one job and the per-session non-terminal limit is checked atomically.

On startup, both stores are loaded before the IPC endpoint is bound. A corrupt, oversized, internally inconsistent or unsupported store fails closed. Pack publication uses a two-phase durable marker: after atomic no-clobber publication, the daemon records the exact archive length and whole-file BLAKE3 before recording terminal success. Recovery reports an interrupted pack as `completed` only when that recorded identity still matches a fully verified archive. A pre-existing archive, an unrecorded publication, a replacement archive or any other interrupted job is conservatively marked `failed` with phase `daemon_restarted`. Existing terminal results are retained.

## Resource limits

Default daemon ceilings are:

| Limit | Ceiling |
|---|---:|
| request frame | 1 MiB |
| concurrent jobs | 4 |
| connections | 32 |
| jobs per session | 128 non-terminal |
| threads per job | 8 |
| memory reservation | 4 GiB |
| temporary bytes | 16 GiB |
| output bytes | 1 TiB |
| `read_range` | 64 MiB |
| retained events per job | 4096 |
| retained jobs/idempotency records | 4096 |

A request may lower its limits but cannot exceed daemon ceilings. Work acquires bounded job, thread and weighted-memory permits before entering the blocking pool. Input/output/temp estimates are checked before execution, and engine-side counters enforce actual input, metadata, temporary and published-output budgets while bytes are processed. Directory scanning charges entries, path storage and pending child buffers before retaining them. Decode limits count all metadata sections together. Metadata-only RPC results are also bounded by both the job output quota and the IPC response frame. Cancellation and deadline checks occur while queued, while waiting for permits and at engine I/O checkpoints.

Pack execution passes the job's memory and temporary-space ceilings as separate
engine limits. Codec tasks are rejected before dispatch when their declared
input, scratch and output bounds do not fit the memory ceiling. Compressed input
is bound to the file identity captured during scan and is rechecked for identity,
length, modification time and BLAKE3 across both read passes.

If a state mutation cannot be durably recorded, the daemon fails closed: it stops accepting requests and cancels running work. Graceful IPC shutdown stops accepting connections, terminates connection tasks, cancels queued/running jobs, waits for terminal persistence and removes the owned UDS endpoint.

## CLI integration

The `pithos` CLI supports `pack`, `unpack`, `list`, `inspect`, `extract`, `verify` and `capabilities` in both `--mode standalone` (the default) and `--mode daemon`. `--daemon-state-dir` selects the local daemon endpoint and is valid only in daemon mode. `--output-format human|json` remains global in both modes. Daemon-mode commands negotiate capabilities, submit jobs, validate response IDs, resume the same session after a transport reconnect, wait for terminal state, and propagate stable public errors.

`extract --stdout` uses bounded `read_range` jobs. Each result digest covers exactly the returned subrange. Before writing bytes to stdout, the CLI verifies that the returned transfer is a regular file directly inside the daemon transfer directory, has the declared length and matches its BLAKE3 digest. Large entries are fetched in multiple independently verified chunks. Capability and resume secrets are removed from public capabilities output.

## Public errors

JSON-RPC standard codes are used for parse, envelope, method and parameter failures. Domain failures use code `-32000` with one stable kind: `invalid_argument`, `unsupported_format`, `unsupported_feature`, `unsafe_path`, `permission_denied`, `resource_limit`, `corrupt_archive`, `integrity_mismatch`, `input_changed`, `job_not_found`, `job_conflict`, `cancelled` or `internal`. Public messages are deliberately generic and do not expose tokens, stack traces or raw filesystem errors.
