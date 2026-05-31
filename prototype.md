# The Substrate Prototype — Implementation Plan

The concrete prototype design for the substrate application we will build. This document covers what we ship as a product — not the research thesis (see `research.md`), not the standalone index library (see `small-world-index.md`). This is the application that uses the index library to deliver value to actual customers.

---

## What this prototype is

A property-data substrate application. It uses the `swindex` library (see `small-world-index.md`) as its query-routing index, layers signed multi-party assertions and verification on top, exposes an open protocol via REST/JSON, Arrow Flight, and MCP, and ingests real-world data from county recorders, MLSs, and other sources. Built in Rust.

It is **the substrate that we operate as a venture**. The first commercial deployment of the substrate concept. The first paying-customer-facing product.

It is **not the index library** (which is a standalone reusable crate any application can use).

It is **not the research thesis** (which justifies the venture).

This document is the build plan for the **application layer**.

---

## Architectural relationship to the index library

```
┌──────────────────────────────────────────────────────────────────────┐
│   substrate-rs  (THIS APPLICATION — what we ship as our product)     │
│                                                                      │
│   • County data ingestion + normalization                            │
│   • Signed assertion / PR / verification workflow                    │
│   • Trust-tier policy enforcement                                    │
│   • Capability tokens (ed25519, scoped, revocable)                   │
│   • Open protocol: REST (axum), Arrow Flight (tonic), MCP (rmcp)     │
│   • Federation between substrate operator instances                  │
│   • Agent-native API + token-optimized response shapes               │
│   • Business logic: title search, maintenance reports, etc.          │
│   • Billing / pricing / metering                                     │
│   • Monitoring / observability                                       │
└──────────────────────┬───────────────────────────────────────────────┘
                       │
                       │ uses
                       ▼
┌──────────────────────────────────────────────────────────────────────┐
│   swindex  (STANDALONE LIBRARY — see small-world-index.md)           │
│                                                                      │
│   • 4-layer small-world property-graph index                         │
│   • Hub detection + Leiden clustering + recursive aggregation        │
│   • O(log N) hub-aware query traversal                               │
│   • Incremental Ada-IVF-style maintenance                            │
│   • Persistence (Fjall hot + Iceberg cold)                           │
│   • Public API: insert_node, query, maintain, query_as_of            │
└──────────────────────┬───────────────────────────────────────────────┘
                       │
                       │ uses
                       ▼
┌──────────────────────────────────────────────────────────────────────┐
│   Foundation crates  (off the shelf)                                 │
│   tokio, arrow-rs, parquet, iceberg-rust, fjall, object_store,       │
│   ed25519-dalek, uuid, hnsw_rs, tantivy, ascent, axum, tonic, rmcp,  │
│   wasmtime, quinn, tracing                                           │
└──────────────────────────────────────────────────────────────────────┘
```

**This is the strict separation.** The substrate application is a customer of the index library. The library knows nothing about real estate, signed assertions, authorities, MCP, or any business semantics. The application knows nothing about Leiden internals or hub detection algorithms — it just calls the library's public API.

---

## Application architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│  CONSUMERS                                                           │
│   Title companies · MLSs · Agents · Owners · Insurers · Auditors    │
│   Real estate platforms · Property managers · Regulators            │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
┌──────────────────────────┴──────────────────────────────────────────┐
│  ACCESS LAYER                                                       │
│   REST/JSON (axum)                                                  │
│   Arrow Flight gRPC (tonic + arrow-flight)                          │
│   MCP server for agents (rmcp)                                      │
│   Token-aligned billing (per LLM-style token)                       │
│   Capability tokens for scoped access                               │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
┌──────────────────────────┴──────────────────────────────────────────┐
│  BUSINESS LOGIC                                                     │
│   Title search service (query + format DAG of ownership chain)      │
│   Maintenance history reports                                       │
│   Contractor reputation lookups                                     │
│   Insurance risk profile                                            │
│   Time-travel queries (as-of any date)                              │
│   Property similarity / comparables                                 │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
┌──────────────────────────┴──────────────────────────────────────────┐
│  WORKFLOW + VERIFICATION                                            │
│   Signed assertion ingest (every write is a signed PR)              │
│   Trust-tier-based merge routing                                    │
│   Agent verifier pipeline (identity, plausibility, anomaly)         │
│   Owner approval flow (push notifications, signing UI)              │
│   Cluster boundary policy (when to re-Leiden)                       │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
┌──────────────────────────┴──────────────────────────────────────────┐
│  IDENTITY + AUTHORITY                                               │
│   UUID7 minting                                                     │
│   ed25519 signing infrastructure                                    │
│   Authority registry (who can sign what, what tier)                 │
│   Capability token issuance + revocation                            │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
┌──────────────────────────┴──────────────────────────────────────────┐
│  INGESTION                                                          │
│   County recorder data feeds (parser per county format)             │
│   MLS feed integrators                                              │
│   CoreLogic / ATTOM bulk data licensing                             │
│   Geospatial overlays (FEMA, USGS, climate, school districts)       │
│   Contractor APIs (when partnerships exist)                         │
│   Owner self-service ingestion (web UI for property history)        │
└──────────────────────────┬──────────────────────────────────────────┘
                           │
                           ▼
                   swindex library
                   (query + maintain)
                           │
                           ▼
                Foundation crates +
              object storage (S3/GCS/etc.)
```

---

## Module layout

```
substrate-rs/
├── Cargo.toml                  # workspace
├── crates/
│   ├── substrate-core/         # public types, errors, traits
│   ├── substrate-identity/     # UUID7, ed25519, authority registry
│   ├── substrate-workflow/     # PR / signed assertion / verification
│   ├── substrate-business/     # title search, maintenance reports, etc.
│   ├── substrate-ingest/       # county recorder + MLS + commercial data
│   │   ├── travis_county.rs    # per-county parsers
│   │   ├── king_county.rs
│   │   ├── reso_upi.rs         # RESO UPI mapping
│   │   └── ...
│   ├── substrate-access/       # axum REST, tonic Arrow Flight, rmcp MCP
│   │   ├── rest.rs
│   │   ├── flight.rs
│   │   ├── mcp.rs
│   │   ├── capability.rs       # capability tokens
│   │   └── token_billing.rs    # token-aligned metering
│   ├── substrate-federation/   # CRDT replication across instances
│   ├── substrate-agent/        # agent-native response shaping
│   ├── substrate-cli/          # operator CLI (admin, ingest, debug)
│   └── substrate-server/       # main binary; wires everything
├── deploy/
│   ├── docker/
│   ├── helm/
│   └── terraform/
└── tests/
    ├── e2e/
    ├── ingest/
    ├── workflow/
    ├── federation/
    └── agent/
```

The application depends on `swindex` (the library — see `small-world-index.md`) as an external crate. It does not contain `swindex`'s code.

### Estimated size

| Crate | Lines |
|---|---|
| substrate-core | ~1,500 |
| substrate-identity | ~1,500 |
| substrate-workflow | ~2,500 |
| substrate-business | ~3,000 |
| substrate-ingest (per county = ~500-1,000) | ~3,000 (first 4 counties) |
| substrate-access | ~3,000 |
| substrate-federation | ~2,500 |
| substrate-agent | ~1,500 |
| substrate-cli | ~1,000 |
| substrate-server | ~500 |
| Tests | ~5,000 |
| **Total application** | **~25,000 lines of Rust** |

Plus `swindex` (the library) at ~15,000 lines. Total system ~40,000 lines of Rust. **Tractable for a 2–3 engineer team in 4–6 months to v0.**

---

## v0 scope (6 weeks, $50–100K, demo-quality)

The smallest credible proof. Demonstrates the architecture works end-to-end on one county's data, with one title-company customer, with one AI agent integration.

### Week 1: Foundations
- Cargo workspace scaffolding
- substrate-identity: UUID7 minting, ed25519 signing, authority registry
- substrate-core: Node, Edge, Fact, Signature, CapabilityToken types
- Integrate swindex library as dependency (use its public API)
- Iceberg/Fjall storage adapters configured for local development
- Basic axum HTTP server with health endpoint

### Week 2: Ingest one county
- substrate-ingest/travis_county.rs: parser for Travis County TX parcel + deed data
- Mint UUID7s for ~250K parcels
- Map to RESO UPI for compatibility
- Sign parcel + deed facts with simulated county recorder key
- Bulk-load into swindex via `insert_node` API
- Verify all nodes queryable by UUID7, RESO UPI, APN, GPS coordinates

### Week 3: Index validation + first queries
- Run swindex build over ingested graph
- Verify small-world topology emerges (Leiden communities + hub detection)
- Implement substrate-business/title_search.rs:
  - Input: address or UUID7
  - Output: signed DAG showing chain of title back to earliest recorded event
- Demo: full title search on 10 properties in under 1 second each

### Week 4: Workflow + verification
- substrate-workflow/pr.rs: signed assertion lifecycle (PROPOSED → VERIFIED → MERGED / REJECTED)
- Trust tiers: simulate authoritative county recorder, simulate licensed contractor, simulate unverified party
- Demo: contractor PR for a repair → owner approval simulation → merge to canonical
- Demo: unsigned PR from unknown party → rejected at gate

### Week 5: Agent integration
- substrate-access/mcp.rs: rmcp-based MCP server
- Expose tools: substrate.get_property, substrate.get_history, substrate.title_chain, substrate.search
- substrate-agent: token-optimized response shapes (compact summaries, references not full payloads)
- Connect a real agent end-to-end; demo answering "find me 3BR houses in 78704 with no flood risk, no liens, recent HVAC"
- Measure token efficiency vs equivalent JSON API approach (target: ≥10× fewer tokens per task)

### Week 6: Federation + polish
- substrate-federation: two substrate instances replicating facts via Arrow Flight + quinn
- Demo: assert fact on instance A → query result includes it on instance B within 100ms LAN
- Documentation: README, API docs, demo script
- Recorded demo video + customer-ready pitch

**Outcome at end of week 6:** working substrate prototype demonstrating end-to-end value on real Travis County data, with a real AI agent integration, ready to show to title companies and investors.

---

## Critical files

The files that carry the most architectural weight and need the most careful design:

- `crates/substrate-core/src/node.rs` — canonical Node type. Get this right; the whole app depends on it.
- `crates/substrate-identity/src/authority.rs` — authority registry. Defines who can sign what at what tier. The trust model lives here.
- `crates/substrate-workflow/src/pr.rs` — signed assertion lifecycle. The integrity guarantee of the whole substrate.
- `crates/substrate-workflow/src/verifier.rs` — agent-driven verification pipeline. The fraud defense.
- `crates/substrate-business/src/title_search.rs` — the first business-value endpoint. Customer-facing.
- `crates/substrate-access/src/mcp.rs` — MCP server. The agent-distribution path.
- `crates/substrate-ingest/src/travis_county.rs` — first county adapter. Reference for all subsequent counties.
- `crates/substrate-federation/src/replication.rs` — cross-instance sync. The federation guarantee.

---

## Public API surface

### REST API (JSON over HTTP)

```
GET    /v1/property/{uuid7 | upi | apn?county=}
       → returns the property's node + immediate edges as JSON-LD

GET    /v1/property/{uuid7}/history?as_of={timestamp}
       → time-travel view of the property

GET    /v1/property/{uuid7}/title-chain
       → signed DAG of ownership history

GET    /v1/property/{uuid7}/maintenance-log
       → all owner-approved maintenance facts

POST   /v1/assert
       Body: { signed fact }
       → submits a proposed branch (PR)

GET    /v1/search?q=...&filters=...
       → property search across the substrate (uses swindex)

SUBSCRIBE /v1/property/{uuid7}/changes
       → SSE/WebSocket stream of new facts about this property

GET    /v1/capabilities/{principal_uuid}
       → what can this principal access?

POST   /v1/capability/grant
       Body: { signed capability token }
       → owner grants scoped access to another principal
```

### Arrow Flight (gRPC)

For bulk data transfer, used by enterprise consumers (title companies, MLSs, data licensees):

```
DoGet(query: SubstrateQuery) → stream of Arrow RecordBatches
DoPut(stream of Arrow batches with signed facts) → ingest channel
DoExchange(...) → bidirectional for real-time subscription
```

### MCP server (for agents)

Tools exposed to any agent via the Model Context Protocol:

```
substrate.get_property(address_or_uuid)
substrate.get_history(uuid7, as_of?)
substrate.search(criteria)
substrate.subscribe(uuid7)
substrate.verify_chain(uuid7)
substrate.estimate_value(uuid7)
substrate.find_similar(uuid7)
substrate.assert_fact(signed_payload)
substrate.title_chain(uuid7)
substrate.maintenance_history(uuid7)
substrate.risk_profile(uuid7)
```

Each tool returns token-optimized JSON shaped for LLM consumption (compact, structured, references-not-payloads where useful).

---

## Verification (end-to-end testing)

The v0 must demonstrate seven things:

1. **Ingest**: 250K Travis County parcels loaded → every node has UUID7, signed by simulated county recorder, queryable by UUID7 / RESO UPI / APN / coordinates.
2. **Topology emerges**: swindex builds index over ingested data → Leiden discovers ~hundreds of neighborhood clusters → hub graph emerges with ~0.5% of nodes as hubs → multi-hop queries run in <100ms.
3. **Workflow integrity**: PR from unsigned party rejected. PR from contractor with valid signature enters PENDING. Owner signs approval. PR transitions to MERGED. Canonical state reflects the new fact. Audit shows the full lineage.
4. **Time-travel**: assert a series of ownership transfers across simulated dates → query `?as_of=` for several intermediate dates → verify correct historical state returned.
5. **Federation**: two substrate instances; fact asserted on instance A; instance B receives via Arrow Flight subscription within 100ms; both serve consistent reads.
6. **Agent integration**: an agent connected via MCP; complex query routed through substrate; verify response token count is 5–20× lower than equivalent SQL/REST approach.
7. **Title search business value**: full title chain for 10 properties returned in <1 second average; output is signed, verifiable, formatted for downstream consumption.

---

## Deferred (post-v0)

These are explicitly NOT in scope for the 6-week prototype but are architecturally accommodated:

- **National rollout.** v0 is one county. National coverage is months of county-by-county integrations.
- **Production-grade authority partnerships.** v0 simulates county recorder signatures. Real partnerships are months of contract negotiation.
- **Production-grade insurance integrations.** v0 demonstrates the model. Real carrier partnerships are post-Series A.
- **Production-grade contractor onboarding.** v0 simulates a single contractor flow. Scaling to thousands is post-Series A.
- **WASM-based code-as-fact behavior nodes.** v0 has the data layer; WASM behaviors layered on later.
- **Multi-region production deployment.** v0 is single-region. Multi-region federation is operational maturity.
- **Compliance certifications** (SOC2, HIPAA-equivalent for regulated data). v0 demonstrates capability-based access; certification is post-product-market-fit.
- **Billing / pricing infrastructure.** v0 is unmetered demo. Real billing is post-customer-validation.

---

## What we use that we don't write

Following the principle that the index library is reusable, and the foundation crates are off-the-shelf:

| Layer | Comes from | What we use it for |
|---|---|---|
| Async runtime | `tokio` | Concurrency for the server |
| HTTP framework | `axum` | REST API server |
| gRPC | `tonic` + `arrow-flight` | Arrow Flight wire protocol |
| MCP server | `rmcp` | Agent integration |
| Identity | `uuid` + `ed25519-dalek` | UUID7 + signed assertions |
| Storage | `iceberg-rust`, `parquet`, `fjall`, `object_store` | Cold + hot persistence |
| Columnar | `arrow-rs` | In-memory format throughout |
| Index | **`swindex` (our library, separate)** | All graph queries |
| Vector index | `hnsw_rs` or `swarc` | Embedding similarity (when needed) |
| Full-text | `tantivy` | Text search |
| Datalog (optional) | `ascent` | Rule-based inference |
| QUIC | `quinn` | Federation transport |
| WASM | `wasmtime` | Future: signed behavior nodes |
| Tracing | `tracing` + `opentelemetry-rust` | Observability |

**Net code we write:** application logic, ingestion adapters, business endpoints, workflow, federation choreography, MCP tool definitions, agent response shaping. About 25K lines of Rust for v0.

---

## Operational considerations (for the v0 demo + post-v0 production)

### Storage
- **Hot**: Fjall keyspaces for catalog + indexes (~10–50 GB per county at v0 scale)
- **Cold**: Iceberg tables on S3 (or local filesystem for v0)
- **Partitioning**: by county for ingest separability; by time for cold-storage retention

### Networking
- **External**: axum + tonic on standard HTTPS ports
- **Internal (federation)**: quinn QUIC for low-latency cross-instance sync
- **Subscriptions**: SSE/WebSocket on REST side; Arrow Flight DoExchange on gRPC side

### Observability
- `tracing` instrumentation throughout
- OpenTelemetry export to a collector
- Metrics: query latency p50/p95/p99, ingest rate, federation lag, cluster maintenance health
- Critical alerts: index integrity, signed-assertion failures, federation divergence

### Deployment
- v0: docker-compose for local development
- Demo: Kubernetes via Helm chart
- Post-v0: terraform module for cloud deployment (GCP first, then AWS)

---

## Funding implication

The prototype targets seed-stage validation:

- **v0 (6 weeks, $50–100K)**: working demo, ready to show to title companies and investors.
- **v0.1 → v1 (6 months, $1–2M with 2–3 engineers)**: harden, sign first title-company partnership, demonstrate cost reduction on real searches.
- **Seed round ($5–10M after v0)**: extends runway to 18 months; team scales to 8–12; multi-county rollout; first paying customers; Series A readiness.

This is a standard infrastructure-startup arc: small initial spend to demonstrate the architecture works, modest team to harden and onboard early customers, larger round once revenue traction validates the model.

---

## Decision points pending

These need to be decided during or shortly after v0:

1. **First production county.** Travis TX, King WA, or Maricopa AZ — depends on which has the most receptive recorder and best-formatted public data.
2. **First title-company partner.** A mid-size, tech-friendly title company willing to pilot the substrate-backed title search. Likely warm-intro driven.
3. **Substrate naming / branding.** "The Substrate" as concept; product brand to be chosen. Trademark search required.
4. **License terms for swindex library.** Apache 2.0 (open ecosystem; we capture value from substrate operation) or BSL (commercial protection but less ecosystem trust). Decision after legal review.
5. **Hosting / DNS for the federation network.** Whose namespace anchors the trust? Initially ours; longer term, foundation governance.

---

## Document hierarchy reminder

- **`research.md`** — venture thesis, market, moat, business model, prior art. Why we are building this.
- **`prototype.md`** (this file) — implementation plan for the substrate application. What we ship.
- **`small-world-index.md`** — the standalone reusable index library used by the prototype. The underlying technology.

These three documents are deliberately separate. They serve different audiences (investors / engineers / library users) and evolve on different schedules. Do not mix them.
