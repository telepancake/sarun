# Chupa ownership boundary

Chupa owns mirror registration, scheduling, driver processes, source
acquisition, construction evidence, publication, archive lookup, HTTP
browsing, and terminal reading. Its durable supervisor authority is
`<state-home>/mirrors.db`; corpus-specific selectors and receipts remain
authoritative inside their driver-owned destinations.

Sarun does not own or reinterpret those facts. It configures Chupa to use the
existing namespaced Sarun state directory, converts Chupa `Job` projections to
Sarun's bounded wire types, and contributes optional capture/image providers
to the gateway. Chupa never imports Sarun types.

The source move does not migrate mirror data. Existing destinations,
generations, selectors, scratch inputs, receipts, and databases stay at their
recorded paths. Sarun therefore sees its prior registrations after the
extraction, while standalone Chupa receives a separate XDG default unless the
operator explicitly selects another state root.

Driver execution remains a supervised child-process boundary. The supervisor
self-executes the current binary with `wikimak`, `ietfmak`, or `gitdepot`; both
the Chupa and Sarun binaries implement that multicall contract. This retains
driver isolation and attributable exit/stderr state without a reverse
dependency.

Site browsing is an extension seam. Standalone Chupa provides an empty capture
provider and can be embedded with any implementation of `CaptureProvider`.
Sarun's adapter projects its captured HTTP responses through that interface.
Image enhancement follows the same pattern and defaults to original images.

Some persisted markers and environment variables still use their historical
`SARUN_*` or `.sarun-*` spellings. They are compatibility surfaces for existing
mirror data and driver processes, not a dependency on Sarun code; new public
configuration is exposed through Chupa APIs and `CHUPA_*` variables.

The standalone GUI is a projection and command surface, never durable
authority. It reads supervisor projections and submits explicit actions. A
closed terminal or failed redraw does not imply that a mirror run stopped.
