// `runner resize` and MCP's resize_vm: a stopped VM's disk, memory and
// vCPUs (Store::resize). systemd is asked first, as `import --replace` asks,
// so a VM that's up or asleep is refused by its state before anything is
// copied: a sleeping VM's snapshot is tied to its disk and its memory. The
// VM's lock, which its boot holds while it's up, is what the store goes by,
// so one started meanwhile is refused too.
use crate::store::{self, Sizes, Store, Vm};
use crate::systemd::{self, State};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] store::Error),
    #[error(transparent)]
    Systemd(#[from] systemd::Error),
    #[error("{vm} is {state}: `runner stop {vm}` first, then resize")]
    Up { vm: String, state: State },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Changes the stopped VM's sizes, each that `sizes` gives, as
/// Store::resize does, and returns the VM with them.
pub fn resize(store: &Store, name: &str, sizes: &Sizes) -> Result<Vm> {
    store.get(name)?;
    let state = systemd::state(store, name)?;
    if state != State::Stopped {
        let vm = name.to_string();
        return Err(Error::Up { vm, state });
    }
    Ok(store.resize(name, sizes)?)
}
