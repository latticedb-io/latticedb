# Identity & Authentication Research — Enterprise DB Survey & Decision Vector

> Discussion Q10 deep-dive: how enterprise databases handle identity, authentication,
> TLS and authorization — findings and a **phased decision vector** for LatticeDB
> (not a scalar decision). LatticeDB project, 2026-11-25.

## 1. Scope

Our containment hierarchy: **Host → Database → Graph schema → forest → node**. Identity
must work at Host level (listeners, subjects), authorization at Database/Graph/subtree
level (kernel scopes, P9), with gateways (pooler, MCP, REST) doing federation. Question:
what do production systems actually do, which mechanisms compose into a future-proof
vector, and what is the minimal-but-correct v1?

## 2. Survey

### 2.1 PostgreSQL (the fullest reference model)
- **Roles are cluster-level** (per server instance), spanning databases; role inheritance;
  memberships. Databases do not own users.
- **pg_hba.conf** — an ordered policy table: `(connection type, database, user, source
  address, method, options)`; first match wins. Methods: trust, scram-sha-256 (default
  since 14), md5 (legacy), password, **cert** (client TLS certificate, standalone or
  combined via `clientcert=verify-ca`), GSSAPI/Kerberos, SSPI, LDAP, RADIUS, PAM.
- **TLS modes**: `require` / `verify-ca` / `verify-full` (chain + hostname). **pg_ident.conf**
  maps certificate subject (CN) → database user.
- **Cloud IAM federation without wire changes**: AWS RDS/Aurora IAM auth generates a
  short-lived (~15 min) signed token *used as the SCRAM password*; Azure Database for PG
  accepts Entra ID tokens; Cloud SQL has IAM auth. The lesson: **the legacy password field
  becomes the token carrier — protocol untouched, federation at the edge**.

### 2.2 MySQL 8
- Pluggable auth: `caching_sha2_password` default; LDAP/PAM/Kerberos plugins (enterprise);
  roles at server level; TLS `verify_identity` mode.
- Lesson: **auth-method plugin architecture** — methods behind one negotiation point.

### 2.3 Microsoft SQL Server
- Windows Integrated Auth (Kerberos/NTLM) first-class in AD estates; SQL logins + users;
  **contained database users** — identity scoped to a single database (portability,
  cloud-first). Azure AD principals.
- Lesson: an alternative to cluster-level subjects: **DB-scoped identities** exist and
  thrive in cloud-native deployments.

### 2.4 MongoDB
- SCRAM-SHA-256 default; **x.509 mutual TLS as first-class identity** (cluster members and
  clients); LDAP/Kerberos (enterprise); **authorization roles scoped per database** plus
  cluster-wide roles; Atlas adds OIDC/federated auth.
- Lesson: DB-scoped RBAC; cert-as-identity natively supported.

### 2.5 Redis 6+ ACL
- Multiple users; per-user: passwords, **command categories** (allow/deny), **key
  patterns** (read/write per glob), channels; TLS; optional client certs.
- Lesson: **fine-grained operation/pattern ACLs enforced in the kernel** — the closest
  existing cousin of our scope model (read-subtree / write-structure / write-attribute /
  invoke-action / administer).

### 2.6 CockroachDB
- TLS certs: node cert; **client cert with CN = user** (cert-as-identity, zero passwords);
  or password auth; cluster roles; web console via sessions/JWT; cloud SSO.
- Lesson: the minimal cert-as-identity path is production-proven.

### 2.7 Kafka (the multi-listener precedent)
- **Each listener carries its own security protocol**: PLAINTEXT / SSL / SASL_SSL;
  SASL mechanisms: PLAIN, SCRAM-SHA-256/512, GSSAPI (Kerberos), **OAUTHBEARER** (broker
  validates JWTs issued by an external IdP); ACLs over (principal, operation, resource
  pattern).
- Lesson: **per-listener auth mapping** — directly our UDS / TCP / gRPC triple — and
  broker-side OIDC token validation is a proven pattern for gateway federation.

### 2.8 SPIFFE / SPIRE
- Workload identity (x509-SVID / JWT-SVID) via attestation + automatic rotation; built
  for **service-to-service** identity inside a trust domain; not for human users.
- Lesson: the right tool for the **pooler→DB hop in Kubernetes**; overkill for single-host
  or embedded use; transport identity only — never a substitute for DB-user scopes.

### 2.9 The proxy tier (PgBouncer & friends)
- Client certificates **cannot be transparently forwarded** by connection poolers;
  SCRAM passthrough requires the proxy to know the password (or fetch a SCRAM secret via
  `auth_query`); robust setups terminate TLS at the proxy and re-authenticate upstream
  with proxy-owned credentials, or re-sign certificates.
- Lesson: **identity translation at the pooler is unavoidable** — which is exactly the
  design we froze (pooler = first-party identity gateway, P9/§6).

## 3. Themes

1. **Auth = policy table, not code.** pg_hba/Kafka-listener model: (listener, transport
   security state, subject, target, source) → (method, restrictions). First match wins.
2. **Methods are pluggable behind one negotiation point** (MySQL plugins; our handshake
   already negotiates capabilities).
3. **Cert-as-identity is legitimate but limited**: CockroachDB (CN=user) and MongoDB
   (x509) prove it; PgBouncer proves certs don't survive proxies — translation required.
4. **Federation without wire changes**: short-lived tokens riding the legacy password
   field (Aurora/Entra) — identity moves at the edge; the kernel stays boring.
5. **Authorization granularity spectrum**: cluster roles + per-DB grants (PG) → DB-scoped
   roles (Mongo, MSSQL contained users) → operation/pattern ACLs (Redis). Ours (scope sets
   bound to graphs/subtrees, journaled allow/deny) sits at the Redis end — the most
   rigorous, and required by semi-autonomous agents.
6. **Service identity ≠ user identity**: SPIFFE for workload hops; SCRAM/scopes for
   database principals; the pooler translates between the two planes.

## 4. LatticeDB decision vector (phased)

### Fixed kernel contract (independent of phase)
- **Subject** = {kind: user | agent | service-token, id}. **Scope set** = {read-subtree,
  write-structure, write-attribute, invoke-action, administer}; grants bind scopes to
  Database / Graph / subtree paths (journaled, audited — P9).
- **Policy table** (host-level kernel config, pg_hba-style, ordered rules, first-match):
  `(listener, database, subject-pattern, source, tls-state) → (method, scope bound)`.
- Host-level subjects (PG cluster-role precedent); DB/graph-level grants. Contained
  DB-scoped subjects (MSSQL pattern) = possible later extension, not v1.
- Authn failures → host security log; in-DB authorization decisions (allow/deny) → journal.

### Phase v1 (beta)
- SCRAM-SHA-256, protocol-native, primary method (verifiers in host-managed secrets file,
  admin-managed — the pg_authid pattern).
- Client-cert method: subject in cert CN/URI + host map file (pg_ident analog).
- UDS: SCM_CREDENTIALS peer (uid,pid) → subject via local map.
- TLS modes require / verify-ca / verify-full per listener policy.
- Library mode = host process = trusted (no network surface).
- Admin gRPC listener: separate admin subjects + mTLS.

### Phase v1.x (federation at gateways)
- OIDC/OAuth2 ingestion at pooler/MCP/REST: token introspection → subject + scopes →
  **short-lived DB token issued by the pooler, carried in the SCRAM password field**
  (Aurora pattern; zero protocol change).
- LDAP bind integration; MCP tool→scope binding enforcement (per the base report, ch. 10).

### Phase v2 (workload identity at scale)
- SPIFFE/SPIRE for in-cluster service identity on the pooler→DB hop; automatic rotation;
  cloud IAM federation (AWS/GCP/Azure) issuing short-lived DB tokens; attestation-backed
  agent identities for MCP clients.

### Decision matrix

| Dimension | v1 | v1.x | v2 |
|---|---|---|---|
| Human users | SCRAM / cert | + OIDC SSO | + IAM federation |
| Agents | scoped subjects + SCRAM/cert | + token exchange, tool-scope binding | + attested workload identity |
| Services | UDS peer map / cert | + pooler-issued short tokens | + SPIFFE mTLS |
| Admin | dedicated subjects + mTLS | + OIDC | + IAM |
| Rotation | manual certs | gateway-issued short tokens | automatic (SPIRE) |

## 5. Sub-decisions pending owner confirmation

- V1 method set = SCRAM + client-cert + UDS peer-cred + policy table — confirm.
- Host-level subjects vs DB-scoped contained subjects — host-level assumed (PG precedent).
- Where the policy table lives: host config file, hot-reloadable — assumed.
