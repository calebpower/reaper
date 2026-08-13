# The provider contract

A **provider** creates, tags, inspects and destroys machines. It is the seam
between `reaper` and whatever actually runs virtual machines.

## Scope: a door left ajar, not a plugin framework

This seam is scoped deliberately narrow, because it is the one most likely to
grow into something nobody asked for:

- **No dynamic loading.** No `dlopen`, no shared objects, no ABI to keep stable.
- **No plugin discovery.** Providers do not register themselves at runtime.
- **A new provider is a new module, compiled in**, selected by name in the site
  configuration.

The benefit of a trait here is not extensibility for its own sake. It is that
hypervisor concepts stay *inside* the hypervisor's module, where a lint guard
can prove it, instead of leaking into session bookkeeping and the CLI where they
would have to be untangled later.

If this design turns out to be wrong, it turns out to be wrong cheaply.

## What a provider must do

| Operation | Contract |
|---|---|
| **Clone** | Create a machine from a named template. Returns a handle |
| **Set expiry** | Record an expiry time on the machine, durably and externally visible -- the sweeper reads it without asking the provider |
| **Start / stop** | Power control |
| **Destroy** | Remove the machine and reclaim its storage |
| **Discover address** | Report the machine's IP, without DNS or mDNS |
| **List** | Enumerate the machines this provider is responsible for |

Two properties matter more than the operation list:

**Expiry must be visible to something that is not the CLI.** The whole
dead-man's-switch design rests on it. The CLI renews an expiry tag while a
session is alive; if the operator vanishes, the tag stops moving and an
independent sweeper collects the machine. A provider that can only express
expiry *inside* reaper's own state has not met this contract, because reaper's
own state dies with the operator.

**Expiry is set immediately after creation, before first boot.** The window
between "machine exists" and "machine has an expiry" is the one unrecoverable
state in the design: a machine with no expiry tag is a machine nothing will ever
collect. Providers must make that window as small as they can, and the sweeper
logs such machines for a human rather than guessing.

## What belongs to a provider, not to the core

Every hypervisor has vocabulary, and all of it stays behind the seam. Numeric
machine identifiers, identifier ranges, resource pools, task or job handles,
authentication schemes, API endpoints, TLS policy -- none of these are core
concepts, and a lint guard fails the build when they appear outside a provider's
module.

**The sweeper is provider-specific too.** It talks directly to a hypervisor's
API to enumerate and destroy machines, and it deliberately does not go through
reaper -- an independent backstop that shared code with the thing it is backing
up would not be independent. So each provider brings its own sweeper, and
`cull/` is organised by provider for that reason.

## The trait, as it stands

```rust
pub trait Provider {
    fn name(&self) -> &'static str;
    fn create(&self, req: &CreateRequest) -> Result<MachineRef>;
    fn set_expiry(&self, machine: &MachineRef, at: SystemTime) -> Result<()>;
    fn start(&self, machine: &MachineRef) -> Result<()>;
    fn stop(&self, machine: &MachineRef) -> Result<()>;
    fn destroy(&self, machine: &MachineRef) -> Result<()>;
    fn address(&self, machine: &MachineRef) -> Result<Option<IpAddr>>;
    fn list(&self) -> Result<Vec<MachineSummary>>;
}
```

`MachineRef` is an opaque string. The core stores it and hands it back; it never
parses it. The moment anything outside a provider reads structure out of that
string, this seam has stopped meaning anything.

`ProviderError` keeps `Timeout` separate from every other failure, and
implementations must respect the distinction. A timeout means the machine's
state is *unknown* -- the operation may still be running -- so a caller must
never respond by destroying things. The expiry tag exists to cover that case.

## Implementations

**Proxmox** -- the implementation that ships. Its module owns machine
identifiers and their permitted range, resource-pool membership, asynchronous
task polling, API token authentication and TLS policy. Its sweeper lives in
`cull/proxmox/`.

**CBSD** -- a documented intention, not an implementation. Nothing in this
repository implements it. It is named here because the seam exists in
anticipation of it, and because a contract written with one implementation in
mind tends to describe that implementation rather than the contract.

Implementing it would mean: a module satisfying the operations above; an expiry
mechanism visible to an external sweeper, which is the requirement most likely
to need thought on a system without an arbitrary-tag facility; a sweeper under
`cull/cbsd/` with its own decision self-test; and a site-configuration section
naming it.

Note the honest weakness while there is exactly one implementation: the lint
guard tests the *shape* of the boundary -- that hypervisor vocabulary has not
leaked -- but it cannot prove the boundary is in the right *place*. Only a
second implementation does that. Building one speculatively, before anyone needs
it, would cost more than it would prove.
